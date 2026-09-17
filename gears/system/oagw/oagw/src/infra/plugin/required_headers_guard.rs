//! `RequiredHeadersGuardPlugin` (ADR 0009).
//!
//! Enforces the *presence* of configured headers on the outbound request and on
//! the upstream response. Absent or blank configuration means the phase is a
//! no-op (fail-open). Only the first missing header is reported per rejection.
//!
//! Config keys (comma-separated header lists):
//!
//! | Key | Phase |
//! |---|---|
//! | `required_request_headers` | `guard_request` |
//! | `required_response_headers` | `guard_response` |

use async_trait::async_trait;

use crate::domain::gts_helpers::REQUIRED_HEADERS_GUARD_PLUGIN_ID;
use crate::domain::plugin::{
    GuardDecision, GuardPlugin, PluginError, RequestContext, ResponseContext,
};

/// Reason code reported when a required header is missing.
pub const REQUIRED_HEADER_MISSING: &str = "REQUIRED_HEADER_MISSING";
/// Request-phase rejection status.
pub const REQUEST_REJECTION_STATUS: u16 = 400;
/// Response-phase rejection status.
pub const RESPONSE_REJECTION_STATUS: u16 = 502;

/// Presence-only header enforcement.
#[derive(Debug, Default, Clone, Copy)]
pub struct RequiredHeadersGuardPlugin;

/// Splits a comma-separated header list, dropping blank entries.
#[must_use]
pub fn parse_header_list(raw: Option<&str>) -> Vec<String> {
    raw.unwrap_or_default()
        .split(',')
        .map(str::trim)
        .filter(|entry| !entry.is_empty())
        .map(str::to_owned)
        .collect()
}

#[async_trait]
impl GuardPlugin for RequiredHeadersGuardPlugin {
    fn id(&self) -> &str {
        REQUIRED_HEADERS_GUARD_PLUGIN_ID
    }

    async fn guard_request(&self, ctx: &RequestContext) -> Result<GuardDecision, PluginError> {
        let required = parse_header_list(
            ctx.config
                .get("required_request_headers")
                .map(String::as_str),
        );
        for name in required {
            if !ctx.has_header(&name) {
                return Ok(GuardDecision::reject(
                    REQUEST_REJECTION_STATUS,
                    REQUIRED_HEADER_MISSING,
                    format!("required request header '{name}' is missing"),
                ));
            }
        }
        Ok(GuardDecision::Allow)
    }

    async fn guard_response(&self, ctx: &ResponseContext) -> Result<GuardDecision, PluginError> {
        let required = parse_header_list(
            ctx.config
                .get("required_response_headers")
                .map(String::as_str),
        );
        for name in required {
            if !ctx.has_header(&name) {
                return Ok(GuardDecision::reject(
                    RESPONSE_REJECTION_STATUS,
                    REQUIRED_HEADER_MISSING,
                    format!("upstream response is missing required header '{name}'"),
                ));
            }
        }
        Ok(GuardDecision::Allow)
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    fn request(config: &[(&str, &str)], headers: &[(&str, &str)]) -> RequestContext {
        RequestContext {
            config: config
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect(),
            headers: headers
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect(),
            ..RequestContext::default()
        }
    }

    #[tokio::test]
    async fn fails_open_when_unconfigured() {
        let plugin = RequiredHeadersGuardPlugin;
        let blank = request(&[("required_request_headers", "  , , ")], &[]);
        assert!(
            plugin
                .guard_request(&blank)
                .await
                .expect("allow")
                .is_allow()
        );
        assert!(
            plugin
                .guard_request(&request(&[], &[]))
                .await
                .expect("allow")
                .is_allow()
        );
    }

    #[tokio::test]
    async fn rejects_on_first_missing_request_header() {
        let plugin = RequiredHeadersGuardPlugin;
        let ctx = request(
            &[("required_request_headers", "x-correlation-id,accept")],
            &[("Accept", "application/json")],
        );
        match plugin.guard_request(&ctx).await.expect("decision") {
            GuardDecision::Reject {
                status,
                error_code,
                message,
            } => {
                assert_eq!(status, 400);
                assert_eq!(error_code, REQUIRED_HEADER_MISSING);
                assert!(message.contains("x-correlation-id"));
            }
            GuardDecision::Allow => panic!("expected rejection"),
        }
    }

    #[tokio::test]
    async fn rejects_on_missing_response_header_with_502() {
        let plugin = RequiredHeadersGuardPlugin;
        let response = ResponseContext {
            status: 200,
            headers: vec![("X-Other".to_owned(), "1".to_owned())],
            config: [(
                "required_response_headers".to_owned(),
                "content-type".to_owned(),
            )]
            .into_iter()
            .collect(),
        };
        match plugin.guard_response(&response).await.expect("decision") {
            GuardDecision::Reject {
                status, error_code, ..
            } => {
                assert_eq!(status, 502);
                assert_eq!(error_code, REQUIRED_HEADER_MISSING);
            }
            GuardDecision::Allow => panic!("expected rejection"),
        }

        let mut ok = ResponseContext {
            status: 200,
            headers: vec![("Content-Type".to_owned(), "text/plain".to_owned())],
            config: [(
                "required_response_headers".to_owned(),
                "content-type".to_owned(),
            )]
            .into_iter()
            .collect(),
        };
        assert!(plugin.guard_response(&ok).await.expect("allow").is_allow());
        ok.remove_header("content-type");
        assert!(!ok.has_header("CONTENT-TYPE"));
    }

    #[test]
    fn header_list_parser_trims_and_skips_blanks() {
        let parsed = parse_header_list(Some(" a , ,, b "));
        assert_eq!(parsed, vec!["a".to_owned(), "b".to_owned()]);
        assert!(parse_header_list(None).is_empty());
        assert!(parse_header_list(Some("")).is_empty());
    }
}
