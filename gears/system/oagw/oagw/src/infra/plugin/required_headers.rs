//! Required-headers guard plugin (ADR-0009).
//!
//! Checks for the presence of configured headers before the upstream call
//! (`required_request_headers`) and on the upstream response
//! (`required_response_headers`). Both keys are comma-separated lists and
//! independent; absent or blank configuration fails open (no-op).

use async_trait::async_trait;
use http::StatusCode;

use crate::domain::models::plugin_gts::GUARD_REQUIRED_HEADERS;
use crate::domain::plugin::{GuardPlugin, PluginError, RequestContext, ResponseContext};

/// Configuration keys (ADR-0009).
pub mod keys {
    pub const REQUIRED_REQUEST_HEADERS: &str = "required_request_headers";
    pub const REQUIRED_RESPONSE_HEADERS: &str = "required_response_headers";
}

/// Parse a comma-separated header list, trimming whitespace and dropping
/// blanks.
fn parse_list(raw: Option<&str>) -> Vec<String> {
    raw.map(|s| {
        s.split(',')
            .map(str::trim)
            .filter(|e| !e.is_empty())
            .map(str::to_ascii_lowercase)
            .collect()
    })
    .unwrap_or_default()
}

fn has_header(headers: &[(String, String)], name: &str) -> bool {
    headers.iter().any(|(n, _)| n.eq_ignore_ascii_case(name))
}

/// Required-headers guard plugin.
#[derive(Debug, Clone, Copy, Default)]
pub struct RequiredHeadersGuardPlugin;

#[async_trait]
impl GuardPlugin for RequiredHeadersGuardPlugin {
    fn id(&self) -> &'static str {
        GUARD_REQUIRED_HEADERS
    }

    async fn guard_request(&self, ctx: &mut RequestContext<'_>) -> Result<(), PluginError> {
        let required = parse_list(
            ctx.config
                .get(keys::REQUIRED_REQUEST_HEADERS)
                .and_then(|v| v.as_str()),
        );
        if required.is_empty() {
            return Ok(()); // fail-open when unconfigured
        }
        let missing: Vec<&String> = required
            .iter()
            .filter(|name| !has_header(&ctx.headers, name))
            .collect();
        if !missing.is_empty() {
            return Err(PluginError::Rejected {
                status: StatusCode::BAD_REQUEST,
                message: format!(
                    "missing required request headers: {}",
                    missing
                        .iter()
                        .map(|s| s.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
                gts_type: None,
            });
        }
        Ok(())
    }

    async fn guard_response(&self, ctx: &mut ResponseContext<'_>) -> Result<(), PluginError> {
        let required = parse_list(
            ctx.config
                .get(keys::REQUIRED_RESPONSE_HEADERS)
                .and_then(|v| v.as_str()),
        );
        if required.is_empty() {
            return Ok(()); // fail-open when unconfigured
        }
        let missing: Vec<&String> = required
            .iter()
            .filter(|name| !has_header(&ctx.headers, name))
            .collect();
        if !missing.is_empty() {
            return Err(PluginError::Rejected {
                status: StatusCode::BAD_GATEWAY,
                message: format!(
                    "upstream response is missing required headers: {}",
                    missing
                        .iter()
                        .map(|s| s.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
                gts_type: None,
            });
        }
        Ok(())
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use crate::domain::plugin::{PluginError, RequestContext, ResponseContext};
    use toolkit_security::SecurityContext;

    fn ctx(scope: &str) -> SecurityContext {
        SecurityContext::builder()
            .subject_id(uuid::Uuid::new_v4())
            .subject_tenant_id(uuid::Uuid::nil())
            .token_scopes(vec![scope.to_owned()])
            .build()
            .expect("valid security context")
    }

    fn request_ctx<'a>(
        security: &'a SecurityContext,
        config: &'a serde_json::Value,
        headers: Vec<(String, String)>,
    ) -> RequestContext<'a> {
        RequestContext {
            security_context: security,
            config,
            method: &http::Method::GET,
            path: "/".to_owned(),
            query: Vec::new(),
            headers,
            body: None,
            alias: "sut",
        }
    }

    fn response_ctx<'a>(
        config: &'a serde_json::Value,
        headers: Vec<(String, String)>,
    ) -> ResponseContext<'a> {
        ResponseContext {
            config,
            status: http::StatusCode::OK,
            headers,
            body: None,
        }
    }

    fn config_json(v: serde_json::Value) -> serde_json::Value {
        v
    }

    #[tokio::test]
    async fn request_guard_rejects_missing_required_headers() {
        let plugin = RequiredHeadersGuardPlugin;
        let security = ctx("oagw.proxy");
        let config = config_json(serde_json::json!({
            "required_request_headers": "X-Api-Key, x-trace-id"
        }));
        let mut rctx = request_ctx(
            &security,
            &config,
            vec![("x-api-key".to_owned(), "k".to_owned())],
        );
        let err = plugin.guard_request(&mut rctx).await.unwrap_err();
        match err {
            PluginError::Rejected {
                status, message, ..
            } => {
                assert_eq!(status, http::StatusCode::BAD_REQUEST);
                assert!(
                    message.contains("x-trace-id"),
                    "missing header listed: {message}"
                );
                assert!(
                    !message.contains("x-api-key"),
                    "present header not listed: {message}"
                );
            }
            other => panic!("expected rejection, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn request_guard_matches_case_insensitively() {
        let plugin = RequiredHeadersGuardPlugin;
        let security = ctx("oagw.proxy");
        let config = config_json(serde_json::json!({
            "required_request_headers": "X-Api-Key, x-trace-id"
        }));
        // Headers present with a different case than the config.
        let mut rctx = request_ctx(
            &security,
            &config,
            vec![
                ("X-API-KEY".to_owned(), "k".to_owned()),
                ("X-Trace-Id".to_owned(), "t".to_owned()),
            ],
        );
        plugin
            .guard_request(&mut rctx)
            .await
            .expect("all present match case-insensitively");
    }

    #[tokio::test]
    async fn request_guard_fails_open_when_unconfigured() {
        let plugin = RequiredHeadersGuardPlugin;
        let security = ctx("oagw.proxy");
        // No config key at all.
        let empty = config_json(serde_json::json!({}));
        let mut rctx = request_ctx(&security, &empty, Vec::new());
        plugin
            .guard_request(&mut rctx)
            .await
            .expect("unconfigured guard is a no-op");

        // Blank value also fails open.
        let blank = config_json(serde_json::json!({ "required_request_headers": "  ,  " }));
        let mut rctx = request_ctx(&security, &blank, Vec::new());
        plugin
            .guard_request(&mut rctx)
            .await
            .expect("blank config is a no-op");
    }

    #[tokio::test]
    async fn response_guard_rejects_missing_response_headers() {
        let plugin = RequiredHeadersGuardPlugin;
        let config = config_json(serde_json::json!({
            "required_response_headers": "Content-Length, x-oagw-trace"
        }));
        let mut rctx = response_ctx(
            &config,
            vec![("content-length".to_owned(), "42".to_owned())],
        );
        let err = plugin.guard_response(&mut rctx).await.unwrap_err();
        match err {
            PluginError::Rejected {
                status, message, ..
            } => {
                assert_eq!(
                    status,
                    http::StatusCode::BAD_GATEWAY,
                    "response gap is a 502"
                );
                assert!(message.contains("x-oagw-trace"));
            }
            other => panic!("expected rejection, got {other:?}"),
        }

        let mut ok_ctx = response_ctx(
            &config,
            vec![
                ("content-length".to_owned(), "42".to_owned()),
                ("X-OAGW-Trace".to_owned(), "1".to_owned()),
            ],
        );
        plugin
            .guard_response(&mut ok_ctx)
            .await
            .expect("all response headers present");
    }
}
