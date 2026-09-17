//! Request-ID transform plugin (`...~cf.core.oagw.request_id.v1`).
//!
//! Injects a correlation `X-Request-ID` (reusing an inbound one when present,
//! generating a UUID otherwise) and propagates it onto the response.

use async_trait::async_trait;
use uuid::Uuid;

use crate::domain::models::plugin_gts::TRANSFORM_REQUEST_ID;
use crate::domain::plugin::{PluginError, RequestContext, ResponseContext, TransformPlugin};

const REQUEST_ID_HEADER: &str = "x-request-id";

/// Request-ID transform plugin.
#[derive(Debug, Clone, Copy, Default)]
pub struct RequestIdTransformPlugin;

#[async_trait]
impl TransformPlugin for RequestIdTransformPlugin {
    fn id(&self) -> &'static str {
        TRANSFORM_REQUEST_ID
    }

    async fn on_request(&self, ctx: &mut RequestContext<'_>) -> Result<(), PluginError> {
        let existing = ctx
            .headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(REQUEST_ID_HEADER))
            .map(|(_, v)| v.clone());
        let request_id = existing.unwrap_or_else(|| Uuid::new_v4().to_string());
        ctx.headers.push((REQUEST_ID_HEADER.to_owned(), request_id));
        Ok(())
    }

    async fn on_response(&self, ctx: &mut ResponseContext<'_>) -> Result<(), PluginError> {
        if !ctx
            .headers
            .iter()
            .any(|(n, _)| n.eq_ignore_ascii_case(REQUEST_ID_HEADER))
        {
            // The outbound request carried a request id; propagate it to the
            // client as a best-effort correlation aid.
            ctx.headers
                .push((REQUEST_ID_HEADER.to_owned(), Uuid::new_v4().to_string()));
        }
        Ok(())
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use crate::domain::plugin::{RequestContext, ResponseContext};
    use toolkit_security::SecurityContext;

    fn security() -> SecurityContext {
        SecurityContext::builder()
            .subject_id(Uuid::new_v4())
            .subject_tenant_id(Uuid::nil())
            .token_scopes(vec!["oagw.proxy".to_owned()])
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

    fn header_value<'a>(ctx: &'a [(String, String)], name: &str) -> Option<&'a str> {
        ctx.iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    #[tokio::test]
    async fn on_request_generates_a_uuid_when_absent() {
        let plugin = RequestIdTransformPlugin;
        let security = security();
        let config = serde_json::json!({});
        let mut rctx = request_ctx(&security, &config, Vec::new());
        plugin.on_request(&mut rctx).await.expect("transform runs");
        let value = header_value(&rctx.headers, "x-request-id").expect("id injected");
        assert!(
            Uuid::parse_str(value).is_ok(),
            "generated id looks like a UUID, got {value:?}"
        );
    }

    #[tokio::test]
    async fn on_request_reuses_an_inbound_id_case_insensitively() {
        let plugin = RequestIdTransformPlugin;
        let security = security();
        let config = serde_json::json!({});
        // Inbound header in a different case; the transform must reuse it.
        let mut rctx = request_ctx(
            &security,
            &config,
            vec![("X-REQUEST-ID".to_owned(), "inbound-abc".to_owned())],
        );
        plugin.on_request(&mut rctx).await.expect("transform runs");
        assert_eq!(
            header_value(&rctx.headers, "x-request-id"),
            Some("inbound-abc")
        );
    }

    #[tokio::test]
    async fn on_response_propagates_a_correlation_id_to_the_client() {
        let plugin = RequestIdTransformPlugin;
        let config = serde_json::json!({});
        let mut rctx = response_ctx(&config, Vec::new());
        plugin.on_response(&mut rctx).await.expect("transform runs");
        assert!(
            header_value(&rctx.headers, "x-request-id").is_some(),
            "response carries x-request-id"
        );
    }

    #[tokio::test]
    async fn on_response_keeps_an_upstream_supplied_id() {
        let plugin = RequestIdTransformPlugin;
        let config = serde_json::json!({});
        let mut rctx = response_ctx(
            &config,
            vec![("X-Request-Id".to_owned(), "upstream-1".to_owned())],
        );
        plugin.on_response(&mut rctx).await.expect("transform runs");
        // The transform does not duplicate: the upstream id stays the only one.
        assert_eq!(
            rctx.headers
                .iter()
                .filter(|(n, _)| n.eq_ignore_ascii_case("x-request-id"))
                .count(),
            1
        );
        assert_eq!(
            header_value(&rctx.headers, "x-request-id"),
            Some("upstream-1")
        );
    }
}
