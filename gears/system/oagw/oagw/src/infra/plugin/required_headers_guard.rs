//! `required_headers` guard plugin (ADR-0009).
//!
//! Presence-only, case-insensitive, fail-open when unconfigured, and it
//! reports the *first* missing header per rejection: `400` on the request
//! phase, `502` on the response phase.

use async_trait::async_trait;
use http::{HeaderMap, StatusCode};

use crate::domain::error::PluginError;
use crate::domain::gts_helpers as gts;
use crate::domain::plugin::{GuardDecision, GuardPlugin, RequestContext, ResponseContext};

/// Stable machine-readable code carried by both phases' rejections.
pub const REQUIRED_HEADER_MISSING: &str = "REQUIRED_HEADER_MISSING";

/// `required_headers` built-in guard plugin. Stateless.
#[derive(Debug, Default)]
pub struct RequiredHeadersGuardPlugin;

/// Parse a comma-separated header list: trim, lowercase, drop empties.
fn parse_list(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(|entry| entry.trim().to_ascii_lowercase())
        .filter(|entry| !entry.is_empty())
        .collect()
}

/// First configured name absent from `headers`, in configuration order.
fn first_missing(headers: &HeaderMap, required: &[String]) -> Option<String> {
    required
        .iter()
        .find(|name| !headers.keys().any(|key| key.as_str() == name.as_str()))
        .cloned()
}

fn decide(headers: &HeaderMap, raw: Option<&str>, reject_with: StatusCode) -> GuardDecision {
    let Some(required) = raw.map(parse_list).filter(|list| !list.is_empty()) else {
        // Absent or all-blank: the phase is a no-op, so adding the plugin to
        // the registry changes nothing for upstreams that do not opt in.
        return GuardDecision::Allow;
    };
    match first_missing(headers, &required) {
        None => GuardDecision::Allow,
        Some(missing) => GuardDecision::reject(
            reject_with,
            REQUIRED_HEADER_MISSING,
            format!("required header '{missing}' is missing"),
        ),
    }
}

#[async_trait]
impl GuardPlugin for RequiredHeadersGuardPlugin {
    fn id(&self) -> &str {
        "required_headers"
    }

    fn plugin_type(&self) -> &str {
        gts::REQUIRED_HEADERS_GUARD_PLUGIN_ID
    }

    async fn guard_request(&self, ctx: &RequestContext) -> Result<GuardDecision, PluginError> {
        Ok(decide(
            &ctx.headers,
            ctx.config
                .get("required_request_headers")
                .and_then(|v| v.as_str()),
            StatusCode::BAD_REQUEST,
        ))
    }

    async fn guard_response(&self, ctx: &ResponseContext) -> Result<GuardDecision, PluginError> {
        Ok(decide(
            &ctx.headers,
            ctx.config
                .get("required_response_headers")
                .and_then(|v| v.as_str()),
            StatusCode::BAD_GATEWAY,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::{REQUIRED_HEADER_MISSING, RequiredHeadersGuardPlugin, parse_list};
    use crate::domain::plugin::{GuardDecision, GuardPlugin, RequestContext, ResponseContext};
    use http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode};
    use serde_json::{Value, json};
    use toolkit_security::SecurityContext;
    use uuid::Uuid;

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in pairs {
            map.insert(
                HeaderName::from_bytes(name.as_bytes()).expect("name"),
                HeaderValue::from_str(value).expect("value"),
            );
        }
        map
    }

    fn request_ctx(config: Value, pairs: &[(&str, &str)]) -> RequestContext {
        RequestContext {
            method: Method::POST,
            path: "/v1/chat".to_owned(),
            query: Vec::new(),
            headers: headers(pairs),
            config: config.as_object().cloned().unwrap_or_default(),
            security_context: SecurityContext::anonymous(),
            alias: "api.openai.com".to_owned(),
            upstream_id: Uuid::new_v4(),
            route_id: None,
        }
    }

    fn response_ctx(config: Value, pairs: &[(&str, &str)]) -> ResponseContext {
        ResponseContext {
            status: StatusCode::OK,
            headers: headers(pairs),
            config: config.as_object().cloned().unwrap_or_default(),
            request_id: None,
        }
    }

    #[test]
    fn list_parsing_trims_lowercases_and_drops_blanks() {
        assert_eq!(
            parse_list(" X-Correlation-Id , Accept ,, "),
            vec!["x-correlation-id", "accept"]
        );
        assert!(parse_list(", , ,").is_empty());
    }

    #[tokio::test]
    async fn unconfigured_is_a_no_op_in_both_phases() {
        let plugin = RequiredHeadersGuardPlugin;
        let req = request_ctx(json!({}), &[]);
        assert_eq!(
            plugin.guard_request(&req).await.expect("decided"),
            GuardDecision::Allow
        );
        let resp = response_ctx(json!({}), &[]);
        assert_eq!(
            plugin.guard_response(&resp).await.expect("decided"),
            GuardDecision::Allow
        );
    }

    #[tokio::test]
    async fn a_blank_list_is_also_a_no_op() {
        let plugin = RequiredHeadersGuardPlugin;
        let req = request_ctx(json!({ "required_request_headers": ", , ," }), &[]);
        assert_eq!(
            plugin.guard_request(&req).await.expect("decided"),
            GuardDecision::Allow
        );
    }

    #[tokio::test]
    async fn request_phase_rejects_with_400_and_names_the_first_missing_header() {
        let plugin = RequiredHeadersGuardPlugin;
        let req = request_ctx(
            json!({ "required_request_headers": "x-correlation-id,accept" }),
            &[("accept", "application/json")],
        );
        match plugin.guard_request(&req).await.expect("decided") {
            GuardDecision::Reject {
                status,
                error_code,
                message,
            } => {
                assert_eq!(status, StatusCode::BAD_REQUEST);
                assert_eq!(error_code, REQUIRED_HEADER_MISSING);
                assert!(message.contains("x-correlation-id"));
            }
            GuardDecision::Allow => panic!("expected a rejection"),
        }
    }

    #[tokio::test]
    async fn header_matching_is_case_insensitive_and_presence_only() {
        let plugin = RequiredHeadersGuardPlugin;
        let req = request_ctx(
            json!({ "required_request_headers": "X-Correlation-ID" }),
            &[("x-correlation-id", "")],
        );
        assert_eq!(
            plugin.guard_request(&req).await.expect("decided"),
            GuardDecision::Allow,
            "an empty value still counts as present"
        );
    }

    #[tokio::test]
    async fn response_phase_rejects_with_502() {
        let plugin = RequiredHeadersGuardPlugin;
        let resp = response_ctx(json!({ "required_response_headers": "content-type" }), &[]);
        match plugin.guard_response(&resp).await.expect("decided") {
            GuardDecision::Reject { status, .. } => {
                assert_eq!(status, StatusCode::BAD_GATEWAY);
            }
            GuardDecision::Allow => panic!("expected a rejection"),
        }
    }

    #[tokio::test]
    async fn the_two_phases_are_independent() {
        let plugin = RequiredHeadersGuardPlugin;
        // Only the response phase is configured: the request phase allows.
        let req = request_ctx(json!({ "required_response_headers": "content-type" }), &[]);
        assert_eq!(
            plugin.guard_request(&req).await.expect("decided"),
            GuardDecision::Allow
        );
    }
}
