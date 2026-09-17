//! Built-in guard plugins (`ADR 0009`).

use async_trait::async_trait;

use crate::domain::error::OagwError;
use crate::domain::model::guard_plugin_ids;
use crate::domain::plugin::{
    GuardDecision, GuardPlugin, Rejection, RequestContext, ResponseContext,
};
use http::StatusCode;

/// Code reported when a required header is absent.
pub const REQUIRED_HEADER_MISSING: &str = "REQUIRED_HEADER_MISSING";

/// Split a comma-separated config value into lowercase header names.
#[must_use]
pub fn parse_header_list(raw: Option<&str>) -> Vec<String> {
    raw.map(|value| {
        value
            .split(',')
            .map(str::trim)
            .filter(|entry| !entry.is_empty())
            .map(str::to_ascii_lowercase)
            .collect()
    })
    .unwrap_or_default()
}

/// Read a string config key, tolerating JSON string values only.
#[must_use]
pub fn config_string<'a>(config: &'a serde_json::Value, key: &str) -> Option<&'a str> {
    config.get(key).and_then(serde_json::Value::as_str)
}

/// Enforces the presence of configured request and response headers
/// (`ADR 0009`). Fails open when unconfigured.
pub struct RequiredHeadersGuardPlugin;

#[async_trait]
impl GuardPlugin for RequiredHeadersGuardPlugin {
    fn id(&self) -> &'static str {
        "required_headers"
    }

    fn plugin_type(&self) -> &str {
        guard_plugin_ids::REQUIRED_HEADERS
    }

    async fn guard_request(&self, ctx: &RequestContext) -> Result<GuardDecision, OagwError> {
        let required = parse_header_list(config_string(&ctx.config, "required_request_headers"));
        for name in required {
            // The caller's own request is what is validated, not the filtered
            // set the upstream will receive: a header the passthrough filter
            // drops was still sent (`ADR 0009`).
            if ctx.inbound_headers.get(&name).is_none() {
                return Ok(GuardDecision::Reject(Rejection {
                    status: StatusCode::BAD_REQUEST,
                    error_code: REQUIRED_HEADER_MISSING.to_owned(),
                    detail: format!("Required header '{name}' is missing"),
                }));
            }
        }
        Ok(GuardDecision::Allow)
    }

    async fn guard_response(&self, ctx: &ResponseContext) -> Result<GuardDecision, OagwError> {
        let required = parse_header_list(config_string(&ctx.config, "required_response_headers"));
        for name in required {
            if ctx.headers.get(&name).is_none() {
                return Ok(GuardDecision::Reject(Rejection {
                    status: StatusCode::BAD_GATEWAY,
                    error_code: REQUIRED_HEADER_MISSING.to_owned(),
                    detail: format!("Required response header '{name}' is missing"),
                }));
            }
        }
        Ok(GuardDecision::Allow)
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn parses_comma_list_case_insensitively() {
        let parsed = parse_header_list(Some(" X-Correlation-Id, accept ,,"));
        assert_eq!(
            parsed,
            vec!["x-correlation-id".to_owned(), "accept".to_owned()]
        );
    }

    #[test]
    fn blank_config_fails_open() {
        assert!(parse_header_list(None).is_empty());
        assert!(parse_header_list(Some("")).is_empty());
        assert!(parse_header_list(Some(" , ")).is_empty());
    }

    #[test]
    fn missing_request_header_is_rejected_with_400() {
        let ctx = RequestContext {
            tenant_id: uuid::Uuid::nil(),
            caller_tenant_id: uuid::Uuid::nil(),
            subject_id: String::new(),
            method: http::Method::GET,
            path: "/".to_owned(),
            query: None,
            headers: http::HeaderMap::default(),
            inbound_headers: http::HeaderMap::default(),
            client_ip: None,
            security: None,
            config: serde_json::json!({ "required_request_headers": "x-correlation-id" }),
            attributes: std::collections::BTreeMap::default(),
        };
        let rejected = tokio::runtime::Runtime::new()
            .map(|rt| rt.block_on(RequiredHeadersGuardPlugin.guard_request(&ctx)))
            .is_ok_and(|decision| matches!(decision, Ok(GuardDecision::Reject(_))));
        assert!(rejected, "missing header must be rejected");
    }
}
