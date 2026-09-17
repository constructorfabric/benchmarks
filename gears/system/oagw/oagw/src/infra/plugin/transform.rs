//! Built-in transform plugin: `request_id`.
//!
//! One transform plugin, three phases: propagate an incoming `X-Request-ID`
//! upstream, stamp the identifier onto the response, and stamp it onto the
//! problem document the gateway reports when the hop fails.
use async_trait::async_trait;

use crate::domain::plugin::{
    ErrorContext, INTERNAL, PluginError, RequestContext, ResponseContext, TransformPlugin,
    problem_type,
};

/// GTS type id of the built-in `request_id` transform plugin.
pub const REQUEST_ID_PLUGIN_TYPE: &str =
    "gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.request_id.v1";
/// Header the plugin propagates.
pub const REQUEST_ID_HEADER: &str = "x-request-id";
/// Attribute the identifier travels in between phases.
pub const REQUEST_ID_ATTRIBUTE: &str = "oagw.request_id";
/// Attribute recording whether the value was inbound or minted here.
pub const REQUEST_ID_SOURCE: &str = "oagw.request_id_source";

/// Propagates an `X-Request-ID` end to end.
///
/// Effective configuration keys:
///
/// | Key | Default | Meaning |
/// |---|---|---|
/// | `header_name` | `x-request-id` | Header to propagate |
/// | `generate` | `true` | Mint a UUIDv4 when the request carries none |
#[derive(Debug, Default)]
pub struct RequestIdTransformPlugin;

impl RequestIdTransformPlugin {
    /// Header the plugin is configured to propagate.
    fn header_name(ctx: &RequestContext) -> String {
        ctx.config_str_or("header_name", REQUEST_ID_HEADER)
            .trim()
            .to_ascii_lowercase()
    }
}

#[async_trait]
impl TransformPlugin for RequestIdTransformPlugin {
    fn id(&self) -> &str {
        "request_id"
    }

    fn plugin_type(&self) -> &str {
        REQUEST_ID_PLUGIN_TYPE
    }

    async fn transform_request(&self, ctx: &mut RequestContext) -> Result<(), PluginError> {
        let name = Self::header_name(ctx);
        let inbound = ctx
            .header(&name)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_owned);
        let request_id = match inbound {
            Some(request_id) => {
                ctx.set_attribute(REQUEST_ID_SOURCE, "inbound");
                request_id
            }
            None if ctx.config_bool_or("generate", true) => {
                ctx.set_attribute(REQUEST_ID_SOURCE, "generated");
                uuid::Uuid::new_v4().to_string()
            }
            None => return Ok(()),
        };
        ctx.set_header(&name, &request_id);
        ctx.set_attribute(REQUEST_ID_ATTRIBUTE, &request_id);
        Ok(())
    }

    async fn transform_response(&self, ctx: &mut ResponseContext) -> Result<(), PluginError> {
        let Some(request_id) = ctx.attributes.get_str(REQUEST_ID_ATTRIBUTE) else {
            return Ok(());
        };
        stamp(&mut ctx.headers, request_id)
    }

    async fn transform_error(&self, ctx: &mut ErrorContext) -> Result<(), PluginError> {
        let Some(request_id) = ctx.attributes.get_str(REQUEST_ID_ATTRIBUTE) else {
            return Ok(());
        };
        stamp(&mut ctx.headers, request_id)
    }
}

/// Write one header value, failing closed when it is not renderable.
fn stamp(headers: &mut http::HeaderMap, value: &str) -> Result<(), PluginError> {
    let name = http::HeaderName::from_static(REQUEST_ID_HEADER);
    let rendered = http::HeaderValue::try_from(value)
        .map_err(|error| PluginError::new(500, problem_type(INTERNAL), error.to_string()))?;
    headers.insert(name, rendered);
    Ok(())
}
