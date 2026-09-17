//! Request-id transform plugin
//! (`gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.request_id.v1`).
//!
//! Propagates `X-Request-Id` end to end:
//!
//! * **request phase** — reuse the caller's `X-Request-Id` when present,
//!   otherwise mint a fresh UUID v4; always make the final value available to
//!   later phases through `ctx.attributes["request_id"]` and to the trace id.
//! * **response phase** — echo the request id back to the caller as
//!   `X-Request-Id` (and set `trace_id` if the data plane has none yet).
//! * **error phase** — same as the response phase, so failures carry the id
//!   the client sent.
//!
//! The propagation header name is configurable via `header_name`
//! (default `x-request-id`).

use async_trait::async_trait;
use http::{HeaderMap, HeaderName, HeaderValue};

use crate::domain::gts_helpers::REQUEST_ID_TRANSFORM_PLUGIN_ID;
use crate::domain::plugin::{
    ErrorContext, PluginConfig, PluginResult, RequestContext, ResponseContext, TransformPhase,
    TransformPlugin,
};

/// Header the request id is carried in.
pub const DEFAULT_REQUEST_ID_HEADER: &str = "x-request-id";

/// The built-in request-id transform plugin.
#[derive(Debug, Clone, Copy, Default)]
pub struct RequestIdTransformPlugin;

impl RequestIdTransformPlugin {
    /// Header name the plugin operates on, honouring a `header_name` override.
    #[must_use]
    pub fn header_name(config: &PluginConfig) -> String {
        config
            .str_field("header_name")
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map_or_else(
                || DEFAULT_REQUEST_ID_HEADER.to_owned(),
                str::to_ascii_lowercase,
            )
    }

    /// Insert (or replace) the request id in a header map.
    ///
    /// # Errors
    /// [`crate::domain::plugin::PluginError`] when the header name is invalid.
    pub fn inject(map: &mut HeaderMap<HeaderValue>, name: &str, value: &str) -> PluginResult<()> {
        let parsed = name.parse::<HeaderName>().map_err(|e| {
            crate::domain::plugin::PluginError::Internal {
                plugin_id: REQUEST_ID_TRANSFORM_PLUGIN_ID.to_owned(),
                detail: format!("invalid header name `{name}`: {e}"),
            }
        })?;
        let parsed_value = HeaderValue::from_str(value).map_err(|e| {
            crate::domain::plugin::PluginError::Internal {
                plugin_id: REQUEST_ID_TRANSFORM_PLUGIN_ID.to_owned(),
                detail: format!("invalid request id `{value}`: {e}"),
            }
        })?;
        map.insert(parsed, parsed_value);
        Ok(())
    }
}

#[async_trait]
impl TransformPlugin for RequestIdTransformPlugin {
    fn id(&self) -> &str {
        REQUEST_ID_TRANSFORM_PLUGIN_ID
    }

    fn plugin_type(&self) -> &str {
        REQUEST_ID_TRANSFORM_PLUGIN_ID
    }

    fn phases(&self) -> &'static [TransformPhase] {
        &[
            TransformPhase::OnRequest,
            TransformPhase::OnResponse,
            TransformPhase::OnError,
        ]
    }

    async fn transform_request(&self, ctx: &mut RequestContext) -> PluginResult<()> {
        let name = Self::header_name(&ctx.config);
        let request_id = ctx
            .attributes
            .get("request_id")
            .cloned()
            .or_else(|| {
                ctx.headers
                    .get(&name)
                    .and_then(|v| v.to_str().ok())
                    .map(str::to_owned)
            })
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        ctx.attributes
            .insert("request_id".to_owned(), request_id.clone());
        if ctx.trace_id.is_none() {
            ctx.trace_id = Some(request_id.clone());
        }
        ctx.set_header(&name, &request_id)
    }

    async fn transform_response(&self, ctx: &mut ResponseContext) -> PluginResult<()> {
        let Some(request_id) = ctx.attributes.get("request_id").cloned() else {
            return Ok(());
        };
        let name = Self::header_name(&ctx.config);
        Self::inject(&mut ctx.headers, &name, &request_id)?;
        if ctx.trace_id.is_none() {
            ctx.trace_id = Some(request_id);
        }
        Ok(())
    }

    async fn transform_error(&self, ctx: &mut ErrorContext) -> PluginResult<()> {
        let name = Self::header_name(&ctx.config);
        let Some(request_id) = ctx.attributes.get("request_id").cloned() else {
            return Ok(());
        };
        Self::inject(&mut ctx.headers, &name, &request_id)?;
        if ctx.trace_id.is_none() {
            ctx.trace_id = Some(request_id);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::plugin::{ErrorContext, PluginConfig, RequestContext, ResponseContext};

    fn config() -> PluginConfig {
        PluginConfig {
            plugin_id: REQUEST_ID_TRANSFORM_PLUGIN_ID.to_owned(),
            position: 0,
            at_upstream_level: false,
            config: serde_json::Value::Null,
        }
    }

    fn request_ctx() -> RequestContext {
        RequestContext {
            method: "GET".to_owned(),
            path: "/".to_owned(),
            query: String::new(),
            headers: HeaderMap::new(),
            body: None,
            downstream_headers: HeaderMap::new(),
            security: toolkit_security::SecurityContext::anonymous(),
            tenant_id: uuid::Uuid::nil(),
            upstream_id: None,
            route_id: None,
            alias: None,
            trace_id: None,
            config: config(),
            attributes: Default::default(),
        }
    }

    fn response_ctx() -> ResponseContext {
        ResponseContext {
            status: 200,
            headers: HeaderMap::new(),
            body: None,
            tenant_id: uuid::Uuid::nil(),
            upstream_id: None,
            route_id: None,
            trace_id: None,
            config: config(),
            attributes: Default::default(),
        }
    }

    fn error_ctx() -> ErrorContext {
        ErrorContext {
            error: crate::domain::error::DomainError::internal("boom"),
            status: 500,
            headers: HeaderMap::new(),
            body: None,
            tenant_id: uuid::Uuid::nil(),
            upstream_id: None,
            trace_id: None,
            config: config(),
            attributes: Default::default(),
        }
    }

    #[tokio::test]
    async fn mints_and_echoes_a_request_id() {
        let plugin = RequestIdTransformPlugin;
        let mut req = request_ctx();
        plugin.transform_request(&mut req).await.unwrap();
        let id = req.attributes["request_id"].clone();
        assert!(!id.is_empty());
        assert_eq!(req.headers.get("x-request-id").unwrap(), id.as_str());

        let mut resp = response_ctx();
        resp.attributes = req.attributes.clone();
        plugin.transform_response(&mut resp).await.unwrap();
        assert_eq!(resp.headers.get("x-request-id").unwrap(), id.as_str());
    }

    #[tokio::test]
    async fn propagates_a_client_supplied_id() {
        let plugin = RequestIdTransformPlugin;
        let mut req = request_ctx();
        req.set_header("x-request-id", "fixed-id").unwrap();
        plugin.transform_request(&mut req).await.unwrap();
        assert_eq!(req.attributes["request_id"], "fixed-id");
    }

    #[tokio::test]
    async fn error_phase_carries_the_id() {
        let plugin = RequestIdTransformPlugin;
        let mut err = error_ctx();
        err.attributes
            .insert("request_id".to_owned(), "abc".to_owned());
        plugin.transform_error(&mut err).await.unwrap();
        assert_eq!(err.headers.get("x-request-id").unwrap(), "abc");
        assert_eq!(err.trace_id.as_deref(), Some("abc"));
    }
}
