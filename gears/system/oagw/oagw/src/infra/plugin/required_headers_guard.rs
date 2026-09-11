//! Built-in required-headers guard plugin.
//!
//! See `docs/ADR/0009-required-headers-guard-plugin.md`.

use async_trait::async_trait;
use http::HeaderMap;

use crate::domain::plugin::{
    GuardPlugin, PluginError, PluginErrorKind, PluginPhase, RequestContext,
};

/// Config key naming the headers required on the request.
const REQUEST_KEY: &str = "required_request_headers";
/// Config key naming the headers required on the response.
const RESPONSE_KEY: &str = "required_response_headers";
/// Machine-readable code surfaced when a header is missing.
pub const REQUIRED_HEADER_MISSING: &str = "REQUIRED_HEADER_MISSING";

/// Stateless presence-only header enforcement.
pub struct RequiredHeadersGuardPlugin;

/// Split a comma-separated header list, trimming and lower-casing each entry.
#[must_use]
pub fn parse_required(value: &serde_json::Value, key: &str) -> Vec<String> {
    value
        .get(key)
        .and_then(serde_json::Value::as_str)
        .map(|raw| {
            raw.split(',')
                .map(str::trim)
                .filter(|entry| !entry.is_empty())
                .map(str::to_ascii_lowercase)
                .collect()
        })
        .unwrap_or_default()
}

/// First required header absent from `headers`, if any.
#[must_use]
pub fn first_missing(required: &[String], headers: &HeaderMap) -> Option<String> {
    required
        .iter()
        .find(|name| !headers.contains_key(name.as_str()))
        .cloned()
}

#[async_trait]
impl GuardPlugin for RequiredHeadersGuardPlugin {
    fn id(&self) -> &'static str {
        "required_headers"
    }

    async fn guard_request(&self, ctx: &RequestContext) -> Result<(), PluginError> {
        let required = parse_required(&ctx.config, REQUEST_KEY);
        if let Some(missing) = first_missing(&required, &ctx.headers) {
            return Err(PluginError::new(
                PluginErrorKind::BadRequest,
                format!("required request header '{missing}' is missing"),
            )
            .with_code(REQUIRED_HEADER_MISSING));
        }
        ctx.record(self.id(), PluginPhase::Request);
        Ok(())
    }

    async fn guard_response(
        &self,
        ctx: &crate::domain::plugin::ResponseContext,
    ) -> Result<(), PluginError> {
        let required = parse_required(&ctx.config, RESPONSE_KEY);
        if let Some(missing) = first_missing(&required, &ctx.headers) {
            return Err(PluginError::new(
                PluginErrorKind::Upstream,
                format!("required response header '{missing}' is missing"),
            )
            .with_code(REQUIRED_HEADER_MISSING));
        }
        ctx.record(self.id(), PluginPhase::Response);
        Ok(())
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use http::HeaderValue;
    use serde_json::json;

    use super::*;
    use crate::domain::plugin::ResponseContext;
    use crate::infra::plugin::test_support::{recorded, request_context, response_context};

    #[test]
    fn parse_required_splits_trims_and_lowercases() {
        let config = json!({ "required_request_headers": " X-A, ,X-B ,, " });
        assert_eq!(
            parse_required(&config, REQUEST_KEY),
            vec!["x-a".to_owned(), "x-b".to_owned()]
        );
    }

    #[test]
    fn parse_required_tolerates_a_missing_or_non_string_key() {
        assert!(parse_required(&json!({}), REQUEST_KEY).is_empty());
        assert!(parse_required(&json!({ REQUEST_KEY: 7 }), REQUEST_KEY).is_empty());
    }

    #[test]
    fn first_missing_reports_the_first_absent_name() {
        let mut headers = HeaderMap::new();
        headers.insert("x-a", HeaderValue::from_static("1"));
        assert_eq!(first_missing(&["x-a".to_owned()], &headers), None);
        assert_eq!(
            first_missing(&["x-a".to_owned(), "x-b".to_owned()], &headers),
            Some("x-b".to_owned())
        );
        assert!(first_missing(&[], &headers).is_none());
    }

    #[tokio::test]
    async fn request_guard_admits_satisfied_headers() {
        let mut ctx = request_context("local");
        ctx.config = json!({ REQUEST_KEY: "x-a, x-b" });
        ctx.headers.insert("x-a", HeaderValue::from_static("1"));
        ctx.headers.insert("x-b", HeaderValue::from_static("2"));

        RequiredHeadersGuardPlugin
            .guard_request(&ctx)
            .await
            .expect("guard admits the request");
        assert_eq!(recorded(&ctx), vec!["required_headers:Request".to_owned()]);
    }

    #[tokio::test]
    async fn request_guard_rejects_a_missing_header_with_the_code() {
        let mut ctx = request_context("local");
        ctx.config = json!({ REQUEST_KEY: "x-a" });

        let err = RequiredHeadersGuardPlugin
            .guard_request(&ctx)
            .await
            .expect_err("the guard rejects the request");
        assert_eq!(err.kind, PluginErrorKind::BadRequest);
        assert_eq!(err.code.as_deref(), Some(REQUIRED_HEADER_MISSING));
        assert!(err.detail.contains("'x-a'"));
        assert!(recorded(&ctx).is_empty(), "nothing is recorded on failure");
    }

    #[tokio::test]
    async fn response_guard_rejects_a_missing_header_as_upstream() {
        let ctx = response_context("local");
        let mut config = serde_json::Map::new();
        config.insert(
            RESPONSE_KEY.to_owned(),
            serde_json::Value::String("x-signature".to_owned()),
        );

        let ctx = ResponseContext {
            config: serde_json::Value::Object(config),
            ..ctx
        };
        let err = RequiredHeadersGuardPlugin
            .guard_response(&ctx)
            .await
            .expect_err("the guard rejects the response");
        assert_eq!(err.kind, PluginErrorKind::Upstream);
        assert_eq!(err.code.as_deref(), Some(REQUIRED_HEADER_MISSING));
    }

    #[tokio::test]
    async fn guards_without_configuration_pass_through() {
        let mut ctx = request_context("local");
        RequiredHeadersGuardPlugin
            .guard_request(&ctx)
            .await
            .expect("an empty config imposes nothing");
        ctx.config = json!({ REQUEST_KEY: "" });
        RequiredHeadersGuardPlugin
            .guard_request(&ctx)
            .await
            .expect("a blank list imposes nothing");
        assert_eq!(
            recorded(&ctx),
            vec![
                "required_headers:Request".to_owned(),
                "required_headers:Request".to_owned()
            ]
        );
    }
}
