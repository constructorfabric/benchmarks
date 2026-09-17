//! Built-in guard plugins and the `GuardPlugin` trait
//! ([ADR-0002](../../../../docs/ADR/0002-plugin-system.md),
//! [ADR-0009](../../../../docs/ADR/0009-required-headers-guard-plugin.md)).
//!
//! A guard validates and may reject. The only built-in guard is
//! [`RequiredHeadersGuardPlugin`], a stateless presence check over the
//! comma-separated `required_request_headers` / `required_response_headers`
//! configuration keys: absent or blank configuration fails open, header names
//! are matched case-insensitively, and a rejection reports the first missing
//! header only.

use async_trait::async_trait;
use http::StatusCode;
use serde_json::Value;

use crate::domain::error::OagwError;

use super::{GuardDecision, PluginType, RequestContext, UpstreamResponseView};

/// The `GuardPlugin` trait: request/response validation that may reject.
///
/// A guard never mutates: it answers [`GuardDecision::Allow`] or
/// [`GuardDecision::Reject`] for the phase it runs in.
#[async_trait]
pub trait GuardPlugin: Send + Sync {
    /// The GTS identifier the plugin is registered under.
    fn id(&self) -> &str;

    /// The kind of the plugin.
    fn plugin_type(&self) -> PluginType;

    /// Validates the request before it is proxied.
    ///
    /// # Errors
    /// Whatever the plugin cannot recover from; a *policy* rejection is not an
    /// error but a [`GuardDecision::Reject`].
    async fn guard_request(&self, ctx: &mut RequestContext) -> Result<GuardDecision, OagwError>;

    /// Validates the upstream's response.
    ///
    /// # Errors
    /// Whatever the plugin cannot recover from; a *policy* rejection is not an
    /// error but a [`GuardDecision::Reject`].
    async fn guard_response(
        &self,
        ctx: &mut RequestContext,
        response: &mut UpstreamResponseView,
    ) -> Result<GuardDecision, OagwError>;
}

/// `required_request_headers` — comma-separated header names checked in
/// `guard_request`.
pub const REQUIRED_REQUEST_HEADERS: &str = "required_request_headers";
/// `required_response_headers` — comma-separated header names checked in
/// `guard_response`.
pub const REQUIRED_RESPONSE_HEADERS: &str = "required_response_headers";
/// `error_code` of a rejection.
pub const REQUIRED_HEADER_MISSING: &str = "REQUIRED_HEADER_MISSING";

/// The stateless required-header guard
/// ([ADR-0009](../../../../docs/ADR/0009-required-headers-guard-plugin.md)).
#[derive(Debug, Default)]
pub struct RequiredHeadersGuardPlugin;

/// Splits a comma-separated configuration value into the header names to check,
/// lowercased and without blank entries.
fn required_headers(config: Option<&Value>, key: &str) -> Vec<String> {
    let Some(raw) = config
        .and_then(|config| config.get(key))
        .and_then(Value::as_str)
    else {
        return Vec::new();
    };
    raw.split(',')
        .map(str::trim)
        .map(str::to_lowercase)
        .filter(|name| !name.is_empty())
        .collect()
}

/// The first name of `required` that `present` does not carry.
///
/// Headers are matched case-insensitively and by presence only.
fn first_missing<'a>(
    required: &[String],
    present: impl Iterator<Item = &'a str>,
) -> Option<String> {
    let present: Vec<String> = present.map(str::to_lowercase).collect();
    required
        .iter()
        .find(|name| !present.contains(name))
        .cloned()
}

#[async_trait]
impl GuardPlugin for RequiredHeadersGuardPlugin {
    fn id(&self) -> &str {
        super::registry::REQUIRED_HEADERS_GUARD_PLUGIN_ID
    }

    fn plugin_type(&self) -> PluginType {
        PluginType::Guard
    }

    async fn guard_request(&self, ctx: &mut RequestContext) -> Result<GuardDecision, OagwError> {
        let required = required_headers(ctx.config.as_ref(), REQUIRED_REQUEST_HEADERS);
        if required.is_empty() {
            return Ok(GuardDecision::Allow);
        }
        let missing = first_missing(
            &required,
            ctx.request_headers.keys().map(http::HeaderName::as_str),
        );
        Ok(match missing {
            Some(missing) => GuardDecision::Reject {
                status: StatusCode::BAD_REQUEST,
                error_code: REQUIRED_HEADER_MISSING.to_owned(),
                message: format!("required request header '{missing}' is missing"),
            },
            None => GuardDecision::Allow,
        })
    }

    async fn guard_response(
        &self,
        ctx: &mut RequestContext,
        response: &mut UpstreamResponseView,
    ) -> Result<GuardDecision, OagwError> {
        let required = required_headers(ctx.config.as_ref(), REQUIRED_RESPONSE_HEADERS);
        if required.is_empty() {
            return Ok(GuardDecision::Allow);
        }
        let missing = first_missing(
            &required,
            response.headers.keys().map(http::HeaderName::as_str),
        );
        Ok(match missing {
            Some(missing) => GuardDecision::Reject {
                status: StatusCode::BAD_GATEWAY,
                error_code: REQUIRED_HEADER_MISSING.to_owned(),
                message: format!("required response header '{missing}' is missing"),
            },
            None => GuardDecision::Allow,
        })
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]
    use http::{HeaderName, HeaderValue, StatusCode};
    use serde_json::Value;
    use toolkit_security::SecurityContext;
    use uuid::Uuid;

    use super::{GuardDecision, REQUIRED_HEADER_MISSING, RequiredHeadersGuardPlugin};
    use crate::infra::plugins::{
        PluginType, RequestContext, UpstreamResponseView, guard::GuardPlugin as _,
    };

    fn security() -> SecurityContext {
        SecurityContext::builder()
            .subject_id(Uuid::now_v7())
            .subject_tenant_id(Uuid::new_v4())
            .build()
            .expect("valid security context")
    }

    fn context(config: Option<Value>) -> RequestContext {
        let mut context = RequestContext::new(security(), Uuid::new_v4(), "/v1/chat");
        context.config = config;
        context
    }

    fn headers(names: &[(&str, &str)]) -> http::HeaderMap {
        let mut headers = http::HeaderMap::new();
        for (name, value) in names {
            headers.insert(
                HeaderName::from_lowercase(name.as_bytes()).unwrap(),
                HeaderValue::from_str(value).unwrap(),
            );
        }
        headers
    }

    async fn request_decision(config: Option<Value>, given: &[(&str, &str)]) -> GuardDecision {
        let mut context = context(config);
        context.request_headers = headers(given);
        RequiredHeadersGuardPlugin
            .guard_request(&mut context)
            .await
            .unwrap()
    }

    async fn response_decision(config: Option<Value>, given: &[&str]) -> GuardDecision {
        let mut context = context(config);
        let mut response = UpstreamResponseView::new(StatusCode::OK);
        for name in given {
            response.headers.insert(
                HeaderName::from_lowercase(name.as_bytes()).unwrap(),
                HeaderValue::from_static("x"),
            );
        }
        RequiredHeadersGuardPlugin
            .guard_response(&mut context, &mut response)
            .await
            .unwrap()
    }

    fn rejection(decision: GuardDecision) -> (StatusCode, String, String) {
        let GuardDecision::Reject {
            status,
            error_code,
            message,
        } = decision
        else {
            panic!("expected a rejection, got {decision:?}");
        };
        (status, error_code, message)
    }

    #[tokio::test]
    async fn absent_config_fails_open_in_both_phases() {
        let request = request_decision(None, &[]).await;
        let response = response_decision(None, &[]).await;
        assert_eq!(request, GuardDecision::Allow);
        assert_eq!(response, GuardDecision::Allow);
    }

    #[tokio::test]
    async fn blank_config_fails_open() {
        for blank in ["", " , ,", "   "] {
            let config = serde_json::json!({ "required_request_headers": blank });
            let decision = request_decision(Some(config), &[]).await;
            assert_eq!(decision, GuardDecision::Allow, "blank config '{blank}'");
        }
    }

    #[tokio::test]
    async fn a_missing_request_header_is_rejected_with_400() {
        let config = serde_json::json!({ "required_request_headers": "x-correlation-id,accept" });
        let decision = request_decision(Some(config), &[("accept", "application/json")]).await;

        let (status, error_code, message) = rejection(decision);
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(error_code, REQUIRED_HEADER_MISSING);
        assert_eq!(
            message,
            "required request header 'x-correlation-id' is missing"
        );
    }

    #[tokio::test]
    async fn headers_are_matched_case_insensitively() {
        let config = serde_json::json!({ "required_request_headers": "X-CORRELATION-ID" });
        let decision = request_decision(Some(config), &[("x-correlation-id", "1")]).await;
        assert_eq!(decision, GuardDecision::Allow);
    }

    #[tokio::test]
    async fn only_the_first_missing_header_is_reported() {
        let config = serde_json::json!({ "required_request_headers": "a,b,c" });
        let (_, _, message) = rejection(request_decision(Some(config), &[]).await);

        assert!(message.contains("'a'"), "message: {message}");
        assert!(!message.contains("'b'"), "message: {message}");
    }

    #[tokio::test]
    async fn a_missing_response_header_is_rejected_with_502() {
        let config = serde_json::json!({ "required_response_headers": "content-type" });
        let (status, error_code, message) = rejection(response_decision(Some(config), &[]).await);

        assert_eq!(status, StatusCode::BAD_GATEWAY);
        assert_eq!(error_code, REQUIRED_HEADER_MISSING);
        assert_eq!(
            message,
            "required response header 'content-type' is missing"
        );
    }

    #[tokio::test]
    async fn the_two_phases_are_independent() {
        let config = serde_json::json!({
            "required_request_headers": "x-correlation-id",
            "required_response_headers": "content-type"
        });

        let request = request_decision(Some(config.clone()), &[("x-correlation-id", "1")]).await;
        assert_eq!(request, GuardDecision::Allow);

        let (status, _, _) = rejection(response_decision(Some(config), &[]).await);
        assert_eq!(status, StatusCode::BAD_GATEWAY);
    }

    #[test]
    fn the_plugin_is_a_guard() {
        let plugin = RequiredHeadersGuardPlugin;
        assert_eq!(plugin.plugin_type(), PluginType::Guard);
        assert_eq!(
            plugin.id(),
            "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v1"
        );
    }
}
