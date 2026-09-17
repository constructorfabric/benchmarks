//! Required-headers guard plugin (ADR 0009).
//!
//! A stateless [`GuardPlugin`] that checks for the *presence* of configured
//! header names on the request (before proxying) and/or on the upstream's
//! response (before returning to the caller), rejecting with a phase-specific
//! status on the first missing header.
//!
//! # Config (`ctx.config` keys, both optional and independent)
//!
//! | key | meaning |
//! |---|---|
//! | `required_request_headers` | comma-separated names checked in `guard_request` |
//! | `required_response_headers` | comma-separated names checked in `guard_response` |
//!
//! Absent or blank config fails **open** (the phase is a no-op). Header names
//! are matched case-insensitively; only presence is checked, never values.
//! Only the *first* missing header is reported per rejection.
//!
//! ```json
//! { "required_request_headers": "x-correlation-id,accept",
//!   "required_response_headers": "content-type" }
//! ```

use async_trait::async_trait;

use crate::domain::gts_helpers::REQUIRED_HEADERS_GUARD_PLUGIN_ID;
use crate::domain::plugin::{
    GuardDecision, GuardPlugin, PluginConfig, PluginResult, RequestContext, ResponseContext,
};

/// Configuration of the required-headers guard.
///
/// Kept as a struct for documentation and OpenAPI purposes; the plugin itself
/// reads the same keys from the raw `ctx.config` value, so an unparseable or
/// absent object fails open rather than erroring.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RequiredHeadersGuardConfig {
    /// Comma-separated header names checked on the request.
    pub required_request_headers: Option<String>,
    /// Comma-separated header names checked on the upstream response.
    pub required_response_headers: Option<String>,
}

impl RequiredHeadersGuardConfig {
    /// Build a configuration from a raw `ctx.config` value.
    #[must_use]
    pub fn from_config(config: &serde_json::Value) -> Self {
        let get = |key: &str| -> Option<String> {
            config.get(key).and_then(|v| v.as_str()).map(str::to_owned)
        };
        Self {
            required_request_headers: get("required_request_headers"),
            required_response_headers: get("required_response_headers"),
        }
    }

    /// Serialise to the `ctx.config` shape.
    #[must_use]
    pub fn to_config(&self) -> serde_json::Value {
        let mut object = serde_json::Map::new();
        if let Some(headers) = &self.required_request_headers {
            object.insert(
                "required_request_headers".to_owned(),
                serde_json::json!(headers),
            );
        }
        if let Some(headers) = &self.required_response_headers {
            object.insert(
                "required_response_headers".to_owned(),
                serde_json::json!(headers),
            );
        }
        serde_json::Value::Object(object)
    }

    /// Parsed request-phase header list (split on `,`, trimmed, lower-cased,
    /// empty entries dropped).
    #[must_use]
    pub fn request_headers(&self) -> Vec<String> {
        split_headers(self.required_request_headers.as_deref())
    }

    /// Parsed response-phase header list.
    #[must_use]
    pub fn response_headers(&self) -> Vec<String> {
        split_headers(self.required_response_headers.as_deref())
    }
}

/// Split a comma-separated header list, trimming, lower-casing and dropping
/// empty entries (`", , ,"` is a no-op, not an error — ADR 0009 "Risks").
#[must_use]
pub fn split_headers(raw: Option<&str>) -> Vec<String> {
    raw.unwrap_or_default()
        .split(',')
        .map(str::trim)
        .map(str::to_ascii_lowercase)
        .filter(|s| !s.is_empty())
        .collect()
}

/// The built-in required-headers guard.
#[derive(Debug, Clone, Copy, Default)]
pub struct RequiredHeadersGuardPlugin;

impl RequiredHeadersGuardPlugin {
    /// Rejection detail for a missing header.
    fn missing(name: &str) -> String {
        missing_request_header_detail(name)
    }
}

/// Rejection detail for a request header the configuration requires.
#[must_use]
pub fn missing_request_header_detail(name: &str) -> String {
    format!("required header `{name}` is missing")
}

/// Rejection detail for an upstream response header that never arrived.
#[must_use]
pub fn missing_response_header_detail(name: &str) -> String {
    format!("upstream response is missing required header `{name}`")
}

#[async_trait]
impl GuardPlugin for RequiredHeadersGuardPlugin {
    fn id(&self) -> &str {
        REQUIRED_HEADERS_GUARD_PLUGIN_ID
    }

    fn plugin_type(&self) -> &str {
        REQUIRED_HEADERS_GUARD_PLUGIN_ID
    }

    async fn guard_request(&self, ctx: &RequestContext) -> PluginResult<GuardDecision> {
        let required = ctx.config.header_list("required_request_headers");
        if required.is_empty() {
            return Ok(GuardDecision::allow());
        }
        match required.iter().find(|name| ctx.header(name).is_none()) {
            None => Ok(GuardDecision::allow()),
            Some(name) => Ok(GuardDecision::reject(
                400,
                crate::infra::plugin::REQUIRED_HEADER_MISSING,
                Self::missing(name),
            )),
        }
    }

    async fn guard_response(&self, ctx: &ResponseContext) -> PluginResult<GuardDecision> {
        let required = ctx.config.header_list("required_response_headers");
        if required.is_empty() {
            return Ok(GuardDecision::allow());
        }
        match required
            .iter()
            .find(|name| !ctx.headers.contains_key(name.as_str()))
        {
            None => Ok(GuardDecision::allow()),
            Some(name) => Ok(GuardDecision::reject(
                502,
                crate::infra::plugin::REQUIRED_HEADER_MISSING,
                missing_response_header_detail(name),
            )),
        }
    }
}

/// Evaluate the request phase against a plain header map — the same logic the
/// trait method uses, exposed for reuse by the data plane.
#[must_use]
pub fn guard_request_with(
    required: &[String],
    headers: &http::HeaderMap<http::HeaderValue>,
) -> Option<(u16, &'static str, String)> {
    required
        .iter()
        .find(|name| !headers.contains_key(name.as_str()))
        .map(|name| {
            (
                400u16,
                crate::infra::plugin::REQUIRED_HEADER_MISSING,
                missing_request_header_detail(name),
            )
        })
}

/// Evaluate the response phase against a raw config value and header map.
#[must_use]
pub fn guard_response_with(
    config: &PluginConfig,
    headers: &http::HeaderMap<http::HeaderValue>,
) -> Option<(u16, &'static str, String)> {
    let required = config.header_list("required_response_headers");
    required
        .iter()
        .find(|name| !headers.contains_key(name.as_str()))
        .map(|name| {
            (
                502u16,
                crate::infra::plugin::REQUIRED_HEADER_MISSING,
                missing_response_header_detail(name),
            )
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::plugin::PluginConfig;

    fn request(headers: &[(&str, &str)]) -> RequestContext {
        let mut map = http::HeaderMap::new();
        for (name, value) in headers {
            map.insert(
                // `from_bytes` accepts mixed case and normalises to lower case,
                // which is what the wire allows.
                http::HeaderName::from_bytes(name.as_bytes()).unwrap(),
                http::HeaderValue::from_str(value).unwrap(),
            );
        }
        RequestContext {
            method: "GET".to_owned(),
            path: "/v1/chat".to_owned(),
            query: String::new(),
            headers: map.clone(),
            body: None,
            downstream_headers: map,
            security: toolkit_security::SecurityContext::anonymous(),
            tenant_id: uuid::Uuid::nil(),
            upstream_id: None,
            route_id: None,
            alias: None,
            trace_id: None,
            config: PluginConfig::default(),
            attributes: Default::default(),
        }
    }

    fn response(headers: &[(&str, &str)]) -> ResponseContext {
        let mut map = http::HeaderMap::new();
        for (name, value) in headers {
            map.insert(
                http::HeaderName::from_lowercase(name.as_bytes()).unwrap(),
                http::HeaderValue::from_str(value).unwrap(),
            );
        }
        ResponseContext {
            status: 200,
            headers: map,
            body: None,
            tenant_id: uuid::Uuid::nil(),
            upstream_id: None,
            route_id: None,
            trace_id: None,
            config: PluginConfig::default(),
            attributes: Default::default(),
        }
    }

    fn config(raw: &serde_json::Value) -> PluginConfig {
        PluginConfig {
            plugin_id: REQUIRED_HEADERS_GUARD_PLUGIN_ID.to_owned(),
            position: 0,
            at_upstream_level: true,
            config: raw.clone(),
        }
    }

    #[tokio::test]
    async fn fail_open_without_config() {
        let plugin = RequiredHeadersGuardPlugin;
        let mut ctx = request(&[]);
        ctx.config = config(&serde_json::Value::Null);
        assert_eq!(
            plugin.guard_request(&ctx).await.unwrap(),
            GuardDecision::allow()
        );
    }

    #[tokio::test]
    async fn rejects_the_first_missing_request_header() {
        let plugin = RequiredHeadersGuardPlugin;
        let mut ctx = request(&[("accept", "application/json")]);
        ctx.config = config(&serde_json::json!({
            "required_request_headers": "x-correlation-id,accept"
        }));
        let decision = plugin.guard_request(&ctx).await.unwrap();
        assert_eq!(decision.rejection_status(), Some(400));
        let GuardDecision::Reject { detail, .. } = decision else {
            panic!("expected a rejection");
        };
        assert!(detail.contains("x-correlation-id"));
        assert!(!detail.contains("accept"));
    }

    #[tokio::test]
    async fn matching_is_case_insensitive() {
        let plugin = RequiredHeadersGuardPlugin;
        let mut ctx = request(&[("X-Correlation-Id", "abc")]);
        ctx.config = config(&serde_json::json!({
            "required_request_headers": "x-correlation-id"
        }));
        assert_eq!(
            plugin.guard_request(&ctx).await.unwrap(),
            GuardDecision::allow()
        );
    }

    #[tokio::test]
    async fn blank_config_is_a_no_op() {
        let plugin = RequiredHeadersGuardPlugin;
        let mut ctx = request(&[]);
        ctx.config = config(&serde_json::json!({"required_request_headers": " , , "}));
        assert_eq!(
            plugin.guard_request(&ctx).await.unwrap(),
            GuardDecision::allow()
        );
        let mut ctx = request(&[]);
        ctx.config = config(&serde_json::json!({"required_request_headers": ""}));
        assert_eq!(
            plugin.guard_request(&ctx).await.unwrap(),
            GuardDecision::allow()
        );
    }

    #[tokio::test]
    async fn response_phase_rejects_with_502() {
        let plugin = RequiredHeadersGuardPlugin;
        let mut ctx = response(&[]);
        ctx.config = config(&serde_json::json!({"required_response_headers": "content-type"}));
        assert_eq!(
            plugin
                .guard_response(&ctx)
                .await
                .unwrap()
                .rejection_status(),
            Some(502)
        );
        let mut ok = response(&[("content-type", "application/json")]);
        ok.config = ctx.config;
        assert_eq!(
            plugin.guard_response(&ok).await.unwrap(),
            GuardDecision::allow()
        );
    }

    #[test]
    fn independent_phases() {
        let cfg = RequiredHeadersGuardConfig::from_config(&serde_json::json!({
            "required_response_headers": "content-type"
        }));
        assert!(cfg.request_headers().is_empty());
        assert_eq!(cfg.response_headers(), vec!["content-type".to_owned()]);
        assert_eq!(cfg.to_config()["required_response_headers"], "content-type");
    }
}
