// Updated: 2026-09-01 by Constructor Tech
//! `required_headers` guard plugin (ADR-0009).
//!
//! The only builtin guard identifier that is bindable via `plugins.items[]`.
//! It checks for the *presence* of configured header names on the request and
//! on the upstream response, case-insensitively, and rejects on the first
//! missing one.
//!
//! Config keys (`ctx.config`), both optional:
//!
//! | Key | Description |
//! |---|---|
//! | `required_request_headers` | comma-separated header names required on the request |
//! | `required_response_headers` | comma-separated header names required on the response |
//!
//! An absent or blank configuration fails **open**: it constrains nothing, so
//! binding the plugin without naming any header is a valid way of declaring the
//! hook without enforcing a rule yet.

use async_trait::async_trait;

use crate::domain::plugin::{
    GuardDecision, GuardPlugin, PluginError, RequestContext, ResponseContext,
};

/// The required-headers guard.
#[derive(Debug, Default)]
pub struct RequiredHeadersGuardPlugin;

/// Parse the comma-separated list form the ADR pins: split on commas, trim,
/// lowercase, drop empties.
#[must_use]
pub fn parse_header_list(raw: Option<&str>) -> Vec<String> {
    raw.unwrap_or_default()
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_ascii_lowercase)
        .collect()
}

fn missing(headers: &http::HeaderMap, required: &[String]) -> Option<String> {
    required
        .iter()
        .find(|name| !headers.contains_key(name.as_str()))
        .cloned()
}

#[async_trait]
impl GuardPlugin for RequiredHeadersGuardPlugin {
    fn id(&self) -> &str {
        "required_headers"
    }

    fn plugin_type(&self) -> &'static str {
        crate::gts::GUARD_REQUIRED_HEADERS
    }

    async fn guard_request(&self, ctx: &RequestContext) -> Result<GuardDecision, PluginError> {
        let config = ctx
            .config
            .get("required_headers")
            .or_else(|| ctx.config.get("plugin"))
            .unwrap_or(&serde_json::Value::Null);
        let required = parse_header_list(
            config
                .get("required_request_headers")
                .and_then(|v| v.as_str()),
        );
        match missing(&ctx.headers, &required) {
            None => Ok(GuardDecision::Allow),
            // Only the first missing header is reported (ADR-0009).
            Some(name) => Ok(GuardDecision::reject(
                http::StatusCode::BAD_REQUEST,
                format!("REQUIRED_HEADER_MISSING:{name}"),
            )),
        }
    }

    async fn guard_response(&self, ctx: &ResponseContext) -> Result<GuardDecision, PluginError> {
        // The response phase has no request config attached; the engine passes
        // the binding's config through on the response context in `config`.
        let config = ctx
            .config
            .get("required_headers")
            .cloned()
            .unwrap_or(serde_json::Value::Null);
        let required = parse_header_list(
            config
                .get("required_response_headers")
                .and_then(|v| v.as_str()),
        );
        match missing(&ctx.headers, &required) {
            None => Ok(GuardDecision::Allow),
            Some(name) => Ok(GuardDecision::reject(
                http::StatusCode::BAD_GATEWAY,
                format!("REQUIRED_HEADER_MISSING:{name}"),
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::infra::plugin::test_support::request_context;
    use http::HeaderValue;

    fn config_map(v: serde_json::Value) -> std::collections::BTreeMap<String, serde_json::Value> {
        let mut m = std::collections::BTreeMap::new();
        m.insert("required_headers".to_owned(), v);
        m
    }

    #[test]
    fn parses_a_comma_separated_list() {
        assert_eq!(
            parse_header_list(Some(" X-A , X-B ,, x-c ")),
            vec!["x-a", "x-b", "x-c"]
        );
        assert!(parse_header_list(None).is_empty());
        assert!(parse_header_list(Some("  , ")).is_empty());
    }

    #[tokio::test]
    async fn absent_config_fails_open() {
        let plugin = RequiredHeadersGuardPlugin;
        let ctx = request_context();
        assert_eq!(
            plugin.guard_request(&ctx).await.unwrap(),
            GuardDecision::Allow
        );
    }

    #[tokio::test]
    async fn missing_request_header_is_rejected_with_400() {
        let plugin = RequiredHeadersGuardPlugin;
        let mut ctx = request_context();
        ctx.config =
            config_map(serde_json::json!({ "required_request_headers": "x-tenant, x-signature" }));
        ctx.headers
            .insert("x-tenant", HeaderValue::from_static("a"));
        let decision = plugin.guard_request(&ctx).await.unwrap();
        assert_eq!(
            decision,
            GuardDecision::reject(
                http::StatusCode::BAD_REQUEST,
                "REQUIRED_HEADER_MISSING:x-signature"
            )
        );
    }

    #[tokio::test]
    async fn presence_only_matching_is_case_insensitive() {
        let plugin = RequiredHeadersGuardPlugin;
        let mut ctx = request_context();
        ctx.config = config_map(serde_json::json!({ "required_request_headers": "X-Tenant" }));
        ctx.headers
            .insert("X-TENANT", HeaderValue::from_static("a"));
        assert_eq!(
            plugin.guard_request(&ctx).await.unwrap(),
            GuardDecision::Allow
        );
    }

    #[tokio::test]
    async fn missing_response_header_is_rejected_with_502() {
        let plugin = RequiredHeadersGuardPlugin;
        let ctx = crate::infra::plugin::test_support::response_context(config_map(
            serde_json::json!({ "required_response_headers": "x-req-id" }),
        ));
        let decision = plugin.guard_response(&ctx).await.unwrap();
        assert_eq!(
            decision,
            GuardDecision::reject(
                http::StatusCode::BAD_GATEWAY,
                "REQUIRED_HEADER_MISSING:x-req-id"
            )
        );
    }
}
