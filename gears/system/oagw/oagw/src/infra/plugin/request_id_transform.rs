// Created: 2026-08-31 by Constructor Tech
//! `RequestIdTransformPlugin` (ADR-0002 "Built-in Plugins", DESIGN §3.2).
//!
//! `X-Request-ID` injection and propagation: a request that arrives without an
//! id leaves the gateway with a generated one, and the id the upstream sent
//! back — generated or replaced — is what the client sees. An id the caller
//! already provided is never overwritten.

use async_trait::async_trait;
use http::HeaderValue;
use uuid::Uuid;

use crate::error::OagwError;
use crate::infra::plugin::traits::{
    ErrorContext, RequestContext, ResponseContext, TransformPlugin,
};

/// GTS id of the built-in request-id transform plugin.
pub const REQUEST_ID_TRANSFORM_PLUGIN_ID: &str =
    "gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.request_id.v1";

/// Header the correlation id travels in.
pub const REQUEST_ID_HEADER: &str = "x-request-id";

/// Injects and propagates `X-Request-ID`.
#[derive(Debug, Default)]
pub struct RequestIdTransformPlugin;

#[async_trait]
impl TransformPlugin for RequestIdTransformPlugin {
    fn id(&self) -> &'static str {
        "request_id"
    }

    fn plugin_type(&self) -> &'static str {
        REQUEST_ID_TRANSFORM_PLUGIN_ID
    }

    async fn transform_request(&self, ctx: &mut RequestContext) -> Result<(), OagwError> {
        if ctx.headers.contains_key(REQUEST_ID_HEADER) {
            return Ok(());
        }
        let id = Uuid::new_v4().to_string();
        let value = HeaderValue::from_str(&id)
            .map_err(|_| OagwError::validation("the generated request id is not a header value"))?;
        ctx.headers.insert(REQUEST_ID_HEADER, value);
        Ok(())
    }

    async fn transform_response(&self, ctx: &mut ResponseContext) -> Result<(), OagwError> {
        if ctx.headers.contains_key(REQUEST_ID_HEADER) {
            return Ok(());
        }
        // The id the upstream sent — generated, forwarded or replaced — is what
        // the caller can correlate its own request against. It is read from the
        // upstream response, not from the client-bound headers, because the
        // response rules may have dropped it.
        let Some(id) = ctx.upstream_headers.get(REQUEST_ID_HEADER).cloned() else {
            return Ok(());
        };
        ctx.headers.insert(REQUEST_ID_HEADER, id);
        Ok(())
    }

    async fn transform_error(&self, _ctx: &mut ErrorContext) -> Result<(), OagwError> {
        // A gateway error is not produced by an upstream, so there is no
        // upstream id to propagate; the problem document keeps the trace id the
        // transport layer already attached.
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::infra::plugin::traits::PluginConfig;

    fn plugin() -> RequestIdTransformPlugin {
        RequestIdTransformPlugin
    }

    #[test]
    fn the_plugin_is_the_documented_built_in() {
        let transform = plugin();
        assert_eq!(transform.id(), "request_id");
        assert_eq!(transform.plugin_type(), REQUEST_ID_TRANSFORM_PLUGIN_ID);
        assert_eq!(
            transform.plugin_type(),
            crate::domain::plugin::PluginKind::Transform.built_in_id("request_id")
        );
    }

    #[tokio::test]
    async fn a_request_without_an_id_gains_one() {
        let mut ctx = test_request(http::HeaderMap::new());
        plugin().transform_request(&mut ctx).await.unwrap();
        let id = ctx.headers.get(REQUEST_ID_HEADER).unwrap();
        assert!(Uuid::parse_str(id.to_str().unwrap()).is_ok(), "id: {id:?}");
    }

    #[tokio::test]
    async fn a_caller_provided_id_is_never_overwritten() {
        let mut inbound = http::HeaderMap::new();
        inbound.insert(REQUEST_ID_HEADER, HeaderValue::from_static("caller-id"));
        let mut ctx = test_request(inbound);
        plugin().transform_request(&mut ctx).await.unwrap();
        assert_eq!(
            ctx.headers.get(REQUEST_ID_HEADER).unwrap(),
            HeaderValue::from_static("caller-id")
        );
    }

    #[tokio::test]
    async fn the_upstream_id_is_propagated_to_the_client() {
        let mut upstream = http::HeaderMap::new();
        upstream.insert(REQUEST_ID_HEADER, HeaderValue::from_static("upstream-id"));
        let mut ctx = test_response(&upstream);
        plugin().transform_response(&mut ctx).await.unwrap();
        assert_eq!(
            ctx.headers.get(REQUEST_ID_HEADER).unwrap(),
            HeaderValue::from_static("upstream-id")
        );
    }

    #[tokio::test]
    async fn a_response_without_an_upstream_id_stays_without_one() {
        let mut ctx = test_response(&http::HeaderMap::new());
        plugin().transform_response(&mut ctx).await.unwrap();
        assert!(ctx.headers.get(REQUEST_ID_HEADER).is_none());
    }

    #[tokio::test]
    async fn an_id_the_response_already_carries_is_kept() {
        let mut upstream = http::HeaderMap::new();
        upstream.insert(REQUEST_ID_HEADER, HeaderValue::from_static("upstream-id"));
        let mut ctx = test_response(&upstream);
        ctx.headers
            .insert(REQUEST_ID_HEADER, HeaderValue::from_static("client-bound"));
        plugin().transform_response(&mut ctx).await.unwrap();
        assert_eq!(
            ctx.headers.get(REQUEST_ID_HEADER).unwrap(),
            HeaderValue::from_static("client-bound")
        );
    }

    fn test_request(inbound: http::HeaderMap) -> RequestContext {
        RequestContext {
            security: toolkit_security::SecurityContext::anonymous(),
            upstream: crate::infra::plugin::traits::UpstreamRef {
                id: Uuid::nil(),
                alias: "api.vendor.com".to_owned(),
            },
            method: http::Method::GET,
            headers: inbound,
            query: String::new(),
            config: PluginConfig::empty(),
        }
    }

    fn test_response(upstream_headers: &http::HeaderMap) -> ResponseContext {
        ResponseContext {
            security: toolkit_security::SecurityContext::anonymous(),
            upstream: crate::infra::plugin::traits::UpstreamRef {
                id: Uuid::nil(),
                alias: "api.vendor.com".to_owned(),
            },
            status: http::StatusCode::OK,
            headers: http::HeaderMap::new(),
            upstream_headers: upstream_headers.clone(),
            config: PluginConfig::empty(),
        }
    }
}
