//! `cf.core.oagw.required_headers.v1` — required-header enforcement.
//!
//! Implements `cpt-cf-oagw-adr-required-headers-guard-plugin`: two
//! independent, comma-separated config keys; matching is case-insensitive and
//! presence-only; an absent or all-blank list is a no-op (fail-open); the
//! first missing name is reported, request-phase misses as `400` and
//! response-phase misses as `502`, both with `REQUIRED_HEADER_MISSING`.

use async_trait::async_trait;
use axum::http::HeaderMap;

use crate::domain::gts_helpers::REQUIRED_HEADERS_GUARD_PLUGIN_ID;
use crate::domain::model::{PluginConfig, config_str};
use crate::domain::plugin::{
    GuardDecision, GuardPlugin, PluginError, RequestContext, ResponseContext,
};

/// Config key checked in `guard_request`.
const REQUEST_KEY: &str = "required_request_headers";
/// Config key checked in `guard_response`.
const RESPONSE_KEY: &str = "required_response_headers";
/// Rejection code emitted in both phases.
const ERROR_CODE: &str = "REQUIRED_HEADER_MISSING";

/// Parse a comma-separated header list: trim, lowercase, drop empties.
fn parse_names(config: &PluginConfig, key: &str) -> Vec<String> {
    config_str(config, key)
        .unwrap_or_default()
        .split(',')
        .map(|name| name.trim().to_ascii_lowercase())
        .filter(|name| !name.is_empty())
        .collect()
}

/// First configured name absent from `headers`, if any.
fn first_missing(headers: &HeaderMap, required: &[String]) -> Option<String> {
    required
        .iter()
        .find(|name| !headers.contains_key(name.as_str()))
        .cloned()
}

/// Stateless required-header guard.
#[derive(Debug, Default, Clone, Copy)]
pub struct RequiredHeadersGuardPlugin;

#[async_trait]
impl GuardPlugin for RequiredHeadersGuardPlugin {
    fn id(&self) -> &str {
        "required_headers"
    }

    fn plugin_type(&self) -> &str {
        REQUIRED_HEADERS_GUARD_PLUGIN_ID
    }

    async fn guard_request(&self, ctx: &RequestContext) -> Result<GuardDecision, PluginError> {
        let required = parse_names(&ctx.config, REQUEST_KEY);
        if required.is_empty() {
            return Ok(GuardDecision::Allow);
        }
        match first_missing(&ctx.headers, &required) {
            None => Ok(GuardDecision::Allow),
            Some(name) => Ok(GuardDecision::reject(
                400,
                ERROR_CODE,
                format!("required request header {name:?} is missing"),
            )),
        }
    }

    async fn guard_response(&self, ctx: &ResponseContext) -> Result<GuardDecision, PluginError> {
        let required = parse_names(&ctx.config, RESPONSE_KEY);
        if required.is_empty() {
            return Ok(GuardDecision::Allow);
        }
        match first_missing(&ctx.headers, &required) {
            None => Ok(GuardDecision::Allow),
            Some(name) => Ok(GuardDecision::reject(
                502,
                ERROR_CODE,
                format!("required response header {name:?} is missing"),
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::{Method, StatusCode};
    use bytes::Bytes;
    use toolkit_security::SecurityContext;
    use uuid::Uuid;

    fn config(pairs: &[(&str, &str)]) -> PluginConfig {
        let mut config = PluginConfig::new();
        for (key, value) in pairs {
            config.insert((*key).to_owned(), serde_json::json!(value));
        }
        config
    }

    fn request(cfg: PluginConfig, headers: &[(&str, &str)]) -> RequestContext {
        let mut map = HeaderMap::new();
        for (name, value) in headers {
            map.insert(
                axum::http::HeaderName::try_from(*name).unwrap(),
                axum::http::HeaderValue::from_str(value).unwrap(),
            );
        }
        RequestContext {
            security_context: SecurityContext::anonymous(),
            method: Method::GET,
            path: "/v1/x".to_owned(),
            query: vec![],
            headers: map,
            body: Bytes::new(),
            config: cfg,
            upstream_alias: "api.example.com".to_owned(),
            upstream_id: Uuid::nil(),
        }
    }

    fn response(cfg: PluginConfig, headers: &[(&str, &str)]) -> ResponseContext {
        let mut map = HeaderMap::new();
        for (name, value) in headers {
            map.insert(
                axum::http::HeaderName::try_from(*name).unwrap(),
                axum::http::HeaderValue::from_str(value).unwrap(),
            );
        }
        ResponseContext {
            status: StatusCode::OK,
            headers: map,
            config: cfg,
            request_headers: HeaderMap::new(),
        }
    }

    #[tokio::test]
    async fn unconfigured_is_a_noop_in_both_phases() {
        let plugin = RequiredHeadersGuardPlugin;
        assert_eq!(
            plugin
                .guard_request(&request(PluginConfig::new(), &[]))
                .await
                .unwrap(),
            GuardDecision::Allow
        );
        assert_eq!(
            plugin
                .guard_response(&response(PluginConfig::new(), &[]))
                .await
                .unwrap(),
            GuardDecision::Allow
        );
    }

    #[tokio::test]
    async fn all_blank_config_fails_open() {
        let plugin = RequiredHeadersGuardPlugin;
        let cfg = config(&[(REQUEST_KEY, ", , ,")]);
        assert_eq!(
            plugin.guard_request(&request(cfg, &[])).await.unwrap(),
            GuardDecision::Allow
        );
    }

    #[tokio::test]
    async fn request_phase_rejects_with_400_on_the_first_missing_name() {
        let plugin = RequiredHeadersGuardPlugin;
        let cfg = config(&[(REQUEST_KEY, "x-correlation-id,accept")]);
        let decision = plugin
            .guard_request(&request(cfg, &[("accept", "application/json")]))
            .await
            .unwrap();
        match decision {
            GuardDecision::Reject {
                status,
                error_code,
                message,
            } => {
                assert_eq!(status, 400);
                assert_eq!(error_code, ERROR_CODE);
                assert!(message.contains("x-correlation-id"), "{message}");
            }
            GuardDecision::Allow => panic!("expected a rejection"),
        }
    }

    #[tokio::test]
    async fn matching_is_case_insensitive_and_presence_only() {
        let plugin = RequiredHeadersGuardPlugin;
        let cfg = config(&[(REQUEST_KEY, "X-Correlation-ID")]);
        assert_eq!(
            plugin
                .guard_request(&request(cfg, &[("x-correlation-id", "")]))
                .await
                .unwrap(),
            GuardDecision::Allow,
            "an empty value still counts as present"
        );
    }

    #[tokio::test]
    async fn response_phase_rejects_with_502() {
        let plugin = RequiredHeadersGuardPlugin;
        let cfg = config(&[(RESPONSE_KEY, "content-type")]);
        let decision = plugin.guard_response(&response(cfg, &[])).await.unwrap();
        match decision {
            GuardDecision::Reject { status, .. } => assert_eq!(status, 502),
            GuardDecision::Allow => panic!("expected a rejection"),
        }
    }

    #[tokio::test]
    async fn phases_are_independent() {
        let plugin = RequiredHeadersGuardPlugin;
        // Only the response phase is configured: the request phase no-ops.
        let cfg = config(&[(RESPONSE_KEY, "content-type")]);
        assert_eq!(
            plugin
                .guard_request(&request(cfg.clone(), &[]))
                .await
                .unwrap(),
            GuardDecision::Allow
        );
        assert!(matches!(
            plugin.guard_response(&response(cfg, &[])).await.unwrap(),
            GuardDecision::Reject { .. }
        ));
    }
}
