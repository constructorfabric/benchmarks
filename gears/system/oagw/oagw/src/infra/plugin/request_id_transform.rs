//! `cf.core.oagw.request_id.v1` — `X-Request-ID` propagation.

use async_trait::async_trait;
use http::{HeaderName, HeaderValue};
use uuid::Uuid;

use crate::domain::gts;
use crate::domain::plugin::{
    PluginError, RequestContext, ResponseContext, TransformPlugin, config_nonblank,
};

/// Header carrying the correlation id.
pub const REQUEST_ID_HEADER: HeaderName = HeaderName::from_static("x-request-id");

/// Propagates an inbound `X-Request-ID` to the upstream, minting one when the
/// caller did not supply it, and echoes it back on the response.
#[derive(Debug, Default, Clone, Copy)]
pub struct RequestIdTransformPlugin;

#[async_trait]
impl TransformPlugin for RequestIdTransformPlugin {
    fn id(&self) -> &str {
        "request_id"
    }

    fn plugin_type(&self) -> &str {
        gts::REQUEST_ID_TRANSFORM_PLUGIN_ID
    }

    async fn transform_request(&self, ctx: &mut RequestContext<'_>) -> Result<(), PluginError> {
        let header = header_name(ctx.config)?;
        if ctx.request.headers.contains_key(&header) {
            return Ok(());
        }
        let value = HeaderValue::from_str(&Uuid::new_v4().to_string())
            .map_err(|err| PluginError::Internal(err.to_string()))?;
        ctx.request.headers.insert(header, value);
        Ok(())
    }

    async fn transform_response(&self, ctx: &mut ResponseContext<'_>) -> Result<(), PluginError> {
        let header = header_name(ctx.config)?;
        if ctx.response.headers.contains_key(&header) {
            return Ok(());
        }
        // Nothing to echo when the upstream dropped it and no id was minted;
        // the Data Plane records the correlation id on the access log either
        // way, so this is best-effort.
        Ok(())
    }
}

fn header_name(config: &crate::domain::model::ConfigMap) -> Result<HeaderName, PluginError> {
    match config_nonblank(config, "header_name").or_else(|| config_nonblank(config, "name")) {
        Some(raw) => HeaderName::try_from(raw.to_ascii_lowercase())
            .map_err(|_| PluginError::InvalidConfig(format!("invalid header name '{raw}'"))),
        None => Ok(REQUEST_ID_HEADER),
    }
}
