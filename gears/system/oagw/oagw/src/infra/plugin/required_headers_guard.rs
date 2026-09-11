//! The `required_headers` built-in guard plugin (ADR-0009).
//!
//! Stateless and fail-open: an absent or blank config turns the phase into a
//! no-op. Header names are matched case-insensitively and only presence is
//! checked; the first missing name is reported.

use async_trait::async_trait;

use crate::domain::error::DomainError;
use crate::domain::gts_helpers::REQUIRED_HEADERS_GUARD_PLUGIN_ID;
use crate::domain::plugin::{GuardPlugin, PluginContext};

/// Splits a comma-separated header list into lower-case, trimmed names.
#[must_use]
pub fn parse_header_list(raw: Option<&str>) -> Vec<String> {
    raw.map_or_else(Vec::new, |value| {
        value
            .split(',')
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .map(str::to_ascii_lowercase)
            .collect()
    })
}

/// The first name in `required` missing from `headers`, if any.
#[must_use]
pub fn first_missing(required: &[String], headers: &http::HeaderMap) -> Option<String> {
    required.iter().find(|name| headers.get(name.as_str()).is_none()).cloned()
}

#[derive(Debug, Default)]
struct GuardConfig {
    request: Vec<String>,
    response: Vec<String>,
}

fn config_for(config: &serde_json::Value) -> GuardConfig {
    GuardConfig {
        request: parse_header_list(
            config
                .get("required_request_headers")
                .and_then(serde_json::Value::as_str),
        ),
        response: parse_header_list(
            config
                .get("required_response_headers")
                .and_then(serde_json::Value::as_str),
        ),
    }
}

/// Enforces the presence of configured request and response headers.
#[derive(Debug, Default)]
pub struct RequiredHeadersGuardPlugin;

#[async_trait]
impl GuardPlugin for RequiredHeadersGuardPlugin {
    fn id(&self) -> &'static str {
        REQUIRED_HEADERS_GUARD_PLUGIN_ID
    }

    async fn guard_request(
        &self,
        _context: &PluginContext,
        config: &serde_json::Value,
        headers: &http::HeaderMap,
    ) -> Result<(), DomainError> {
        let config = config_for(config);
        if config.request.is_empty() {
            return Ok(());
        }
        match first_missing(&config.request, headers) {
            None => Ok(()),
            Some(name) => Err(DomainError::Validation(format!(
                "required request header `{name}` is missing"
            ))),
        }
    }

    async fn guard_response(
        &self,
        _context: &PluginContext,
        config: &serde_json::Value,
        _status: http::StatusCode,
        headers: &mut http::HeaderMap,
    ) -> Result<(), DomainError> {
        let config = config_for(config);
        if config.response.is_empty() {
            return Ok(());
        }
        match first_missing(&config.response, headers) {
            None => Ok(()),
            Some(name) => Err(DomainError::DownstreamError(format!(
                "upstream response is missing required header `{name}`"
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(pairs: &[(&str, &str)]) -> http::HeaderMap {
        let mut map = http::HeaderMap::new();
        for (name, value) in pairs {
            map.insert(
                http::HeaderName::from_bytes(name.as_bytes()).expect("valid name"),
                http::HeaderValue::from_str(value).expect("valid value"),
            );
        }
        map
    }

    #[test]
    fn list_parsing_trims_and_lowercases() {
        assert_eq!(
            parse_header_list(Some(" X-Correlation-Id, accept ,,")),
            vec!["x-correlation-id", "accept"]
        );
        assert!(parse_header_list(Some("  , , ")).is_empty());
        assert!(parse_header_list(None).is_empty());
    }

    #[test]
    fn first_missing_reports_only_the_first() {
        let required = vec!["x-a".to_owned(), "x-b".to_owned()];
        let present = headers(&[("x-a", "1")]);
        assert_eq!(first_missing(&required, &present).as_deref(), Some("x-b"));
    }

    #[tokio::test]
    async fn absent_config_fails_open() {
        let plugin = RequiredHeadersGuardPlugin;
        let mut headers = headers(&[]);
        plugin
            .guard_request(
                &context(),
                &serde_json::json!({}),
                &mut headers,
            )
            .await
            .expect("fail-open");
    }

    #[tokio::test]
    async fn missing_request_header_is_a_400() {
        let plugin = RequiredHeadersGuardPlugin;
        let mut headers = headers(&[("accept", "application/json")]);
        let error = plugin
            .guard_request(
                &context(),
                &serde_json::json!({"required_request_headers": "x-correlation-id,accept"}),
                &mut headers,
            )
            .await
            .expect_err("missing");
        assert_eq!(error.status(), 400);
        assert!(error.to_string().contains("x-correlation-id"));
    }

    #[tokio::test]
    async fn all_present_is_allowed() {
        let plugin = RequiredHeadersGuardPlugin;
        let mut headers = headers(&[("x-correlation-id", "abc"), ("accept", "json")]);
        plugin
            .guard_request(
                &context(),
                &serde_json::json!({"required_request_headers": "x-correlation-id,accept"}),
                &mut headers,
            )
            .await
            .expect("present");
    }

    #[tokio::test]
    async fn missing_response_header_is_a_502() {
        let plugin = RequiredHeadersGuardPlugin;
        let mut headers = headers(&[]);
        let error = plugin
            .guard_response(
                &context(),
                &serde_json::json!({"required_response_headers": "content-type"}),
                http::StatusCode::OK,
                &mut headers,
            )
            .await
            .expect_err("missing");
        assert_eq!(error.status(), 502);
    }

    fn context() -> PluginContext {
        PluginContext {
            tenant_id: uuid::Uuid::nil(),
            subject_id: uuid::Uuid::nil(),
            upstream_id: uuid::Uuid::nil(),
            route_id: None,
            alias: "api.openai.com".into(),
            bearer_token: None,
            request_id: None,
        }
    }
}
