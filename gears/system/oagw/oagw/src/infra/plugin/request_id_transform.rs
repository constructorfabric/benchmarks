//! `gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.request_id.v1` —
//! `X-Request-ID` propagation.
//!
//! On request: keep the caller's id, or mint one. On response: echo whatever
//! id the request carried so a client can correlate without parsing the body.

use async_trait::async_trait;
use http::HeaderName;
use http::header::HeaderValue;
use uuid::Uuid;

use crate::domain::gts_helpers::REQUEST_ID_TRANSFORM_PLUGIN_ID;
use crate::domain::plugin::{
    PluginError, PluginResult, RequestContext, ResponseContext, TransformPlugin,
};
use crate::util::REQUEST_ID_HEADER;

pub struct RequestIdTransformPlugin;

fn header_name(configured: Option<&str>) -> PluginResult<HeaderName> {
    let raw = configured.unwrap_or(REQUEST_ID_HEADER);
    HeaderName::try_from(raw).map_err(|_| {
        PluginError::Config(format!("request_id plugin: invalid header name '{raw}'"))
    })
}

#[async_trait]
impl TransformPlugin for RequestIdTransformPlugin {
    fn id(&self) -> &str {
        "request_id"
    }

    fn plugin_type(&self) -> &str {
        REQUEST_ID_TRANSFORM_PLUGIN_ID
    }

    fn phases(&self) -> &[&str] {
        &["on_request", "on_response"]
    }

    async fn transform_request(&self, ctx: &mut RequestContext) -> PluginResult<()> {
        let name = header_name(ctx.config_str("header"))?;
        let existing = ctx
            .headers
            .get(&name)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned)
            .or_else(|| ctx.request_id.clone());

        let value = existing.unwrap_or_else(|| Uuid::new_v4().to_string());
        let header_value = HeaderValue::from_str(&value).map_err(|_| {
            PluginError::Config("request_id plugin: request id is not a valid header value".to_owned())
        })?;
        ctx.headers.insert(name, header_value);
        ctx.request_id = Some(value);
        Ok(())
    }

    async fn transform_response(&self, ctx: &mut ResponseContext) -> PluginResult<()> {
        let name = header_name(ctx.config_str("header"))?;
        if ctx.headers.contains_key(&name) {
            return Ok(());
        }
        let Some(request_id) = ctx.request_id.clone() else {
            return Ok(());
        };
        let header_value = HeaderValue::from_str(&request_id).map_err(|_| {
            PluginError::Config(
                "request_id plugin: request id is not a valid header value".to_owned(),
            )
        })?;
        ctx.headers.insert(name, header_value);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_and_custom_header_names() {
        assert_eq!(header_name(None).expect("default"), REQUEST_ID_HEADER);
        assert_eq!(
            header_name(Some("x-correlation-id")).expect("custom"),
            "x-correlation-id"
        );
        assert!(header_name(Some("bad header")).is_err());
    }
}
