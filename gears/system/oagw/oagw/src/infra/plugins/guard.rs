//! Builtin guard plugins (ADR 0009).
//!
//! [`RequiredHeadersGuardPlugin`] enforces the presence of configured
//! header names on the request (400) and/or the upstream response (502),
//! failing open when unconfigured. Only the first missing header is
//! reported per rejection, with the stable error code
//! `REQUIRED_HEADER_MISSING` in the detail message.

use async_trait::async_trait;
use http::header::HeaderMap;

use crate::domain::plugin::{GuardDecision, GuardPlugin, PluginContext, PluginError};
use crate::gts_helpers;

/// Guard — required-header enforcement (request + response phases).
pub struct RequiredHeadersGuardPlugin;

impl RequiredHeadersGuardPlugin {
    /// Check the phase-specific config key against `headers`.
    fn check(
        ctx: &PluginContext,
        headers: &HeaderMap,
        config_key: &str,
        phase: &str,
        status: u16,
    ) -> GuardDecision {
        let Some(raw) = ctx.config.get(config_key).and_then(|v| v.as_str()) else {
            // Unconfigured phase → fail open.
            return GuardDecision::Continue;
        };
        for name in raw.split(',').map(str::trim).filter(|s| !s.is_empty()) {
            let found = headers
                .keys()
                .any(|k| k.as_str().eq_ignore_ascii_case(name));
            if !found {
                return GuardDecision::Reject {
                    status,
                    detail: format!(
                        "REQUIRED_HEADER_MISSING: required {phase} header '{name}' is missing"
                    ),
                };
            }
        }
        GuardDecision::Continue
    }
}

#[async_trait]
impl GuardPlugin for RequiredHeadersGuardPlugin {
    fn id(&self) -> &'static str {
        gts_helpers::GUARD_PLUGIN_REQUIRED_HEADERS
    }

    fn plugin_type(&self) -> &'static str {
        "guard"
    }

    async fn guard_request(
        &self,
        ctx: &PluginContext,
        headers: &HeaderMap,
    ) -> Result<GuardDecision, PluginError> {
        Ok(Self::check(
            ctx,
            headers,
            "required_request_headers",
            "request",
            400,
        ))
    }

    async fn guard_response(
        &self,
        ctx: &PluginContext,
        headers: &HeaderMap,
    ) -> Result<GuardDecision, PluginError> {
        Ok(Self::check(
            ctx,
            headers,
            "required_response_headers",
            "response",
            502,
        ))
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::Arc;
    use toolkit_security::SecurityContext;

    fn ctx(config: serde_json::Value) -> PluginContext {
        static CLIENT: std::sync::OnceLock<toolkit_http::HttpClient> = std::sync::OnceLock::new();
        let http = CLIENT
            .get_or_init(|| toolkit_http::HttpClient::new().expect("test http client"))
            .clone();
        PluginContext {
            security_context: SecurityContext::anonymous(),
            cred_store: Arc::new(credstore_sdk::test_util::MockCredStoreClient::empty()),
            http,
            config,
        }
    }

    fn headers(map: &[(&'static str, &'static str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in map {
            h.insert(*k, http::header::HeaderValue::from_static(v));
        }
        h
    }

    #[tokio::test]
    async fn unconfigured_phase_fails_open() {
        let plugin = RequiredHeadersGuardPlugin;
        let h = headers(&[("x-correlation-id", "abc")]);
        // No config → nothing required.
        assert_eq!(
            plugin.guard_request(&ctx(json!({})), &h).await.unwrap(),
            GuardDecision::Continue
        );
        assert_eq!(
            plugin.guard_response(&ctx(json!({})), &h).await.unwrap(),
            GuardDecision::Continue
        );
    }

    #[tokio::test]
    async fn request_phase_rejects_first_missing_with_400() {
        let plugin = RequiredHeadersGuardPlugin;
        let h = headers(&[("accept", "application/json")]);
        let cfg = ctx(json!({ "required_request_headers": "x-correlation-id,accept" }));
        let decision = plugin.guard_request(&cfg, &h).await.unwrap();
        match decision {
            GuardDecision::Reject { status, detail } => {
                assert_eq!(status, 400);
                assert!(detail.contains("REQUIRED_HEADER_MISSING"));
                assert!(detail.contains("x-correlation-id"));
            }
            GuardDecision::Continue => panic!("expected rejection"),
        }
    }

    #[tokio::test]
    async fn response_phase_rejects_with_502() {
        let plugin = RequiredHeadersGuardPlugin;
        let h = headers(&[]);
        let cfg = ctx(json!({ "required_response_headers": "content-type" }));
        let decision = plugin.guard_response(&cfg, &h).await.unwrap();
        match decision {
            GuardDecision::Reject { status, .. } => assert_eq!(status, 502),
            GuardDecision::Continue => panic!("expected rejection"),
        }
    }

    #[tokio::test]
    async fn phase_configs_are_independent_and_case_insensitive() {
        let plugin = RequiredHeadersGuardPlugin;
        // Request phase configured, response phase unconfigured.
        let cfg = ctx(json!({ "required_request_headers": "X-CORRELATION-ID" }));
        let h = headers(&[("x-correlation-id", "abc")]);
        assert_eq!(
            plugin.guard_request(&cfg, &h).await.unwrap(),
            GuardDecision::Continue
        );
        assert_eq!(
            plugin.guard_response(&cfg, &h).await.unwrap(),
            GuardDecision::Continue
        );
    }
}
