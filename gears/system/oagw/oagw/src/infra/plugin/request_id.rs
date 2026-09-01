//! Built-in request id transform plugin
//! (`gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.request_id.v1`).
//!
//! `X-Request-ID` propagation: the header name and the propagation semantics
//! come from ADR-0002 ("RequestIdTransformPlugin: X-Request-ID propagation").
//!
//! * Request phase — an inbound `X-Request-ID` is propagated unchanged;
//!   otherwise a fresh UUID is minted. Either way the value is recorded on
//!   [`RequestContext::request_id`] and stamped on the outbound request, so
//!   the upstream sees exactly one id and the gateway can correlate.
//! * Response phase — the response carries the propagated id when the upstream
//!   did not set one itself.
//! * Error phase — the problem document carries the id as well, so a rejected
//!   request stays correlatable end to end.

use async_trait::async_trait;
use axum::http::{HeaderName, HeaderValue};
use uuid::Uuid;

use crate::domain::error::OagwError;
use crate::domain::plugin::{
    ErrorContext, RequestContext, ResponseContext, TRANSFORM_PLUGIN_TYPE_ID, TransformPlugin,
    builtin,
};

/// Header the request id is propagated in.
pub const REQUEST_ID_HEADER: &str = "x-request-id";

/// Request id propagation plugin.
#[derive(Debug, Default, Clone, Copy)]
pub struct RequestIdTransformPlugin;

impl RequestIdTransformPlugin {
    /// Registry key of this plugin.
    pub const PLUGIN_ID: &'static str = builtin::REQUEST_ID_TRANSFORM;

    /// GTS base type of this plugin.
    pub const PLUGIN_TYPE: &'static str = TRANSFORM_PLUGIN_TYPE_ID;
}

fn header_name() -> HeaderName {
    HeaderName::from_static(REQUEST_ID_HEADER)
}

fn stamp(ctx: &mut RequestContext, request_id: &str) {
    let Ok(value) = HeaderValue::from_str(request_id) else {
        return;
    };
    let name = header_name();
    ctx.headers.insert(name.clone(), value.clone());
    ctx.inject_header(name, value);
}

#[async_trait]
impl TransformPlugin for RequestIdTransformPlugin {
    fn id(&self) -> &str {
        Self::PLUGIN_ID
    }

    fn plugin_type(&self) -> &str {
        Self::PLUGIN_TYPE
    }

    async fn transform_request(&self, ctx: &mut RequestContext) -> Result<(), OagwError> {
        let propagated = ctx
            .header(REQUEST_ID_HEADER)
            .and_then(|value| value.to_str().ok())
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
            .unwrap_or_else(|| Uuid::new_v4().to_string());
        ctx.request_id = Some(propagated.clone());
        stamp(ctx, &propagated);
        Ok(())
    }

    async fn transform_response(&self, ctx: &mut ResponseContext) -> Result<(), OagwError> {
        let Some(request_id) = ctx.request_id.clone() else {
            return Ok(());
        };
        if ctx.has_header(REQUEST_ID_HEADER) {
            return Ok(());
        }
        let Ok(value) = HeaderValue::from_str(&request_id) else {
            return Ok(());
        };
        ctx.headers.insert(header_name(), value);
        Ok(())
    }

    async fn transform_error(&self, ctx: &mut ErrorContext) -> Result<(), OagwError> {
        let Some(request_id) = ctx.request_id.clone() else {
            return Ok(());
        };
        let Ok(value) = HeaderValue::from_str(&request_id) else {
            return Ok(());
        };
        ctx.headers.insert(header_name(), value);
        Ok(())
    }
}

#[cfg(test)]
#[path = "request_id_tests.rs"]
mod tests;
